// ------------ Clip Recorder ------------
// Records the game window to an MP4 for the overlay's clip button. Frames come from
// Windows Graphics Capture and go through a Media Foundation encoder, with audio mixed
// in through WASAPI by the audio module below. Only runs on Windows.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use serde_json::json;
use tauri::{AppHandle, Emitter};

use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncCommonRateControlMode_UnconstrainedVBR,
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize, ICodecAPI,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample,
    IMFSinkWriter, MFAudioFormat_AAC, MFAudioFormat_PCM, MFMediaType_Audio, MFMediaType_Video,
    MFNominalRange_16_235, MFTranscodeContainerType_MPEG4, MFVideoFormat_AV1, MFVideoFormat_H264,
    MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
    MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
    MF_TRANSCODE_CONTAINERTYPE,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;

use super::capture::Region;

const RING: usize = 6;
const REGION_CHECK: std::time::Duration = std::time::Duration::from_secs(1);
const SLEEP_GAP_100NS: u64 = 50_000_000;
const AUDIO_CHUNK_FRAMES: usize = audio::SAMPLE_RATE as usize;
const AUDIO_LEAD_SECONDS: u64 = 5;
const AUDIO_READY_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

const STATE_IDLE: u8 = 0;
const STATE_STARTING: u8 = 1;
const STATE_RECORDING: u8 = 2;
const STATE_STOPPING: u8 = 3;

pub struct RecorderStatus {
    state: AtomicU8,
    started_ms: AtomicU64,
    active: parking_lot::Mutex<Option<PathBuf>>,
}

impl RecorderStatus {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_IDLE),
            started_ms: AtomicU64::new(0),
            active: parking_lot::Mutex::new(None),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_RECORDING
    }

    pub fn is_idle(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_IDLE
    }

    pub fn toggle_starts(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            STATE_IDLE | STATE_STOPPING
        )
    }

    pub fn active_path(&self) -> Option<PathBuf> {
        self.active.lock().clone()
    }

    pub fn started_ms(&self) -> u64 {
        self.started_ms.load(Ordering::Relaxed)
    }
}

pub struct AudioSource {
    pub kind: audio::SourceKind,
    pub name: String,
}

pub struct RecSettings {
    pub height: u32,
    pub fps: u32,
    pub quality: String,
    pub directory: PathBuf,
    pub audio: Vec<AudioSource>,
    pub warnings: Vec<String>,
    pub codec: String,
}

pub(super) fn recording_supported() -> bool {
    mf::ready()
}

fn audio_warning(name: &str, error: &str) -> String {
    format!("No {name} audio in this clip: {error}")
}

// ------------ Recorder Thread ------------
// The overlay talks to one long-lived recorder thread through a RecorderHandle. It
// sends toggle/stop commands and reads the shared status to know if a clip is running.
enum Command {
    Toggle {
        hwnd: i64,
        game_id: String,
        settings: RecSettings,
    },
    Stop,
    Shutdown,
}

pub struct RecorderHandle {
    tx: Sender<Command>,
    pub status: Arc<RecorderStatus>,
}

impl RecorderHandle {
    pub fn toggle(&self, hwnd: i64, game_id: &str, settings: RecSettings) {
        let _ = self.tx.send(Command::Toggle {
            hwnd,
            game_id: game_id.to_string(),
            settings,
        });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub fn stop_and_wait(&self, timeout: std::time::Duration) -> bool {
        if self.status.is_idle() {
            return true;
        }
        self.stop();
        let deadline = std::time::Instant::now() + timeout;
        while !self.status.is_idle() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }
}

impl Drop for RecorderHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

pub fn start(app: AppHandle) -> RecorderHandle {
    let (tx, rx) = std::sync::mpsc::channel();
    let status = Arc::new(RecorderStatus::new());
    let thread_status = status.clone();

    let spawned = std::thread::Builder::new()
        .name("overlay-recorder".into())
        .spawn(move || {
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if mf::ready() {
                run(&app, &rx, &thread_status);
            } else {
                log::warn!("recorder: Media Foundation is not present, recording is unavailable");
            }
            unsafe { CoUninitialize() };
        });
    if let Err(e) = spawned {
        log::error!("recorder: could not start its thread: {e}");
    }

    RecorderHandle { tx, status }
}

fn run(app: &AppHandle, rx: &Receiver<Command>, status: &Arc<RecorderStatus>) {
    let _ = unsafe { mf::start_up() };
    loop {
        match rx.recv() {
            Ok(Command::Toggle {
                hwnd,
                game_id,
                settings,
            }) => {
                status.state.store(STATE_STARTING, Ordering::Release);
                let _ = app.emit("overlay-record-starting", json!({ "gameId": game_id }));
                let begun = Session::begin(hwnd, &game_id, &settings);
                let (cancelled, shutdown) = pending_while_starting(rx);
                match begun {
                    Ok(session) if cancelled || shutdown => {
                        abandon(app, status, session, &game_id)
                    }
                    Ok(session) => record(app, rx, status, session, &game_id),
                    Err(e) => {
                        status.state.store(STATE_IDLE, Ordering::Release);
                        log::warn!("recorder: could not start ({e})");
                        let _ = app.emit(
                            "overlay-record-failed",
                            json!({ "gameId": game_id, "error": e }),
                        );
                    }
                }
                if shutdown {
                    break;
                }
            }
            Ok(Command::Stop) => {}
            Ok(Command::Shutdown) | Err(_) => break,
        }
    }
    unsafe { mf::shut_down() };
}

fn pending_while_starting(rx: &Receiver<Command>) -> (bool, bool) {
    use std::sync::mpsc::TryRecvError;
    let mut cancelled = false;
    loop {
        match rx.try_recv() {
            Ok(Command::Toggle { .. }) | Ok(Command::Stop) => cancelled = true,
            Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => return (cancelled, true),
            Err(TryRecvError::Empty) => return (cancelled, false),
        }
    }
}

fn abandon(app: &AppHandle, status: &Arc<RecorderStatus>, mut session: Session, game_id: &str) {
    status.state.store(STATE_STOPPING, Ordering::Release);
    let _ = session.finish();
    let partial = session.partial.clone();
    let thumb = session.thumb.clone();
    drop(session);
    discard(&partial);
    discard(&thumb);
    status.state.store(STATE_IDLE, Ordering::Release);
    log::info!("recorder: cancelled, it was toggled again while starting");
    let _ = app.emit("overlay-record-cancelled", json!({ "gameId": game_id }));
}

fn record(
    app: &AppHandle,
    rx: &Receiver<Command>,
    status: &Arc<RecorderStatus>,
    mut session: Session,
    game_id: &str,
) {
    *status.active.lock() = Some(session.path.clone());
    status.state.store(STATE_RECORDING, Ordering::Release);
    status.started_ms.store(super::now_ms().max(0) as u64, Ordering::Relaxed);
    log::info!("recorder: recording {}", session.describe());
    for warning in &session.warnings {
        log::warn!("recorder: {warning}");
    }
    let _ = app.emit(
        "overlay-record-started",
        json!({
            "path": session.path.to_string_lossy(),
            "audioWarnings": session.warnings,
        }),
    );

    let outcome = session.pump(rx);
    status.state.store(STATE_STOPPING, Ordering::Release);
    let finished = session.finish();
    let path = session.path.clone();
    let partial = session.partial.clone();
    let thumb = session.thumb.clone();
    let stats = session.frame_stats();
    drop(session);

    let saved = match finished {
        Ok(duration) => match super::fs_util::finalize_replace(&partial, &path) {
            Ok(()) => Ok((duration, path)),
            Err(e) => {
                log::warn!("recorder: {e}");
                rescue_clip(&partial, &path, &thumb, game_id)
                    .map(|saved| (duration, saved))
                    .map_err(|e| (e, true))
            }
        },
        Err(e) => {
            discard(&partial);
            discard(&thumb);
            Err((e, false))
        }
    };
    status.state.store(STATE_IDLE, Ordering::Release);
    status.active.lock().take();

    match saved {
        Ok((duration, path)) => {
            let reason = match outcome {
                Ok(reason) => reason,
                Err(e) => {
                    log::warn!("recorder: {e}");
                    Some(format!(
                        "Recording stopped early ({e}). The clip was saved up to that point."
                    ))
                }
            };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            log::info!(
                "recorder: saved {} ({:.1}s, {:.1} MB, {stats})",
                path.display(),
                duration as f64 / 1000.0,
                size as f64 / 1_000_000.0
            );
            let _ = app.emit(
                "overlay-record-stopped",
                json!({
                    "path": path.to_string_lossy(),
                    "durationMs": duration,
                    "reason": reason,
                }),
            );
        }
        Err((e, kept)) => {
            if kept {
                log::warn!("recorder: the clip was kept under its temporary name ({e})");
                if let Err(write) = &outcome {
                    log::warn!("recorder: {write}");
                }
            } else {
                log::warn!("recorder: the clip was discarded ({e})");
            }
            let error = match outcome {
                Err(write) if !kept => {
                    log::warn!("recorder: {write}");
                    format!("Recording failed ({write}). Nothing was saved.")
                }
                _ => e,
            };
            let _ = app.emit(
                "overlay-record-failed",
                json!({ "gameId": game_id, "error": error }),
            );
        }
    }
}

fn discard(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::debug!("recorder: could not remove {}: {e}", path.display()),
    }
}

fn denied_by_windows(error: &str) -> bool {
    error.contains(&windows::Win32::Foundation::E_ACCESSDENIED.to_string())
}

// ------------ Partial Clip Recovery ------------
// Clips are written to a .partial file first and only renamed once they finish. These
// helpers name those files and rescue or clean up the ones a crash left behind.
fn partial_path(path: &Path, attempt: usize) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{attempt}.partial"));
    path.with_file_name(name)
}

fn is_partial_name(name: &str) -> bool {
    name.ends_with(".partial") && name.contains(".mp4.")
}

const FINISHED_SUFFIX: &str = ".finished";

fn finished_marker(partial: &Path) -> PathBuf {
    let mut name = partial.file_name().unwrap_or_default().to_os_string();
    name.push(FINISHED_SUFFIX);
    partial.with_file_name(name)
}

fn intended_name(partial_name: &str) -> Option<&str> {
    let stem = partial_name.strip_suffix(".partial")?;
    let (name, attempt) = stem.rsplit_once('.')?;
    (attempt.parse::<usize>().is_ok() && name.ends_with(".mp4")).then_some(name)
}

fn move_thumb(thumb: &Path, clip: &Path) {
    let Some(name) = super::capture::thumb_path(Path::new(""), clip)
        .file_name()
        .map(|n| n.to_os_string())
    else {
        return;
    };
    let target = thumb.with_file_name(name);
    if target != thumb && thumb.is_file() {
        if let Err(e) = std::fs::rename(thumb, &target) {
            log::debug!("recorder: could not move the thumbnail: {e}");
        }
    }
}

fn rescue_clip(
    partial: &Path,
    path: &Path,
    thumb: &Path,
    game_id: &str,
) -> Result<PathBuf, String> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let fresh = super::capture::capture_path(directory, game_id, "mp4");
    match super::fs_util::finalize_replace(partial, &fresh) {
        Ok(()) => {
            log::info!("recorder: saved the clip as {} instead", fresh.display());
            move_thumb(thumb, &fresh);
            return Ok(fresh);
        }
        Err(e) => log::warn!("recorder: {e}"),
    }
    if let Err(e) = std::fs::write(finished_marker(partial), b"") {
        log::warn!(
            "recorder: could not mark {} as finished: {e}",
            partial.display()
        );
    }
    Err(format!(
        "The clip was recorded, but Peebify could not give it its final name. It is saved as {}.",
        partial.display()
    ))
}

fn has_moov(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    let mut offset = 0u64;
    for _ in 0..64 {
        if offset.saturating_add(8) > len || file.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        let mut header = [0u8; 8];
        if file.read_exact(&mut header).is_err() {
            return false;
        }
        if &header[4..] == b"moov" {
            return true;
        }
        let size = match u32::from_be_bytes([header[0], header[1], header[2], header[3]]) {
            0 => return false,
            1 => {
                let mut large = [0u8; 8];
                if file.read_exact(&mut large).is_err() {
                    return false;
                }
                u64::from_be_bytes(large)
            }
            size => u64::from(size),
        };
        if size < 8 {
            return false;
        }
        offset = offset.saturating_add(size);
    }
    false
}

fn sweep_partials(directory: &Path, root: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(partial_name) = name.strip_suffix(FINISHED_SUFFIX) {
            if is_partial_name(partial_name) && !directory.join(partial_name).exists() {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        if !is_partial_name(&name) {
            continue;
        }
        let partial = entry.path();
        let marker = finished_marker(&partial);
        if marker.is_file() || has_moov(&partial) {
            let Some(intended) = intended_name(&name) else {
                log::warn!("recorder: kept {name}, a finished clip with an unexpected name");
                continue;
            };
            let target = super::capture::unique_path(directory, intended);
            match std::fs::rename(&partial, &target) {
                Ok(()) => {
                    log::info!("recorder: recovered {name} as {}", target.display());
                    let _ = std::fs::remove_file(&marker);
                    move_thumb(
                        &super::capture::thumb_path(root, &directory.join(intended)),
                        &target,
                    );
                }
                Err(e) => log::warn!(
                    "recorder: kept {name}, a finished clip that could not be renamed: {e}"
                ),
            }
            continue;
        }
        match std::fs::remove_file(&partial) {
            Ok(()) => {
                log::info!("recorder: removed {name}, left by a recording that never finished")
            }
            Err(e) => log::debug!("recorder: could not remove {name}: {e}"),
        }
    }
}

fn is_disk_full(e: &windows::core::Error) -> bool {
    matches!(e.code().0 as u32, 0x8007_0070 | 0x8007_0027)
}

fn frames_since(qpc_100ns: u64, origin: u64) -> u64 {
    qpc_100ns.saturating_sub(origin) * audio::SAMPLE_RATE as u64 / 10_000_000
}

fn audio_target(elapsed: u64, written: u64, flush: bool, cap: Option<u64>) -> u64 {
    let rate = audio::SAMPLE_RATE as u64;
    let wanted = if flush {
        elapsed
    } else {
        elapsed.saturating_sub(rate / 4)
    };
    let wanted = cap.map_or(wanted, |cap| wanted.min(cap));
    wanted.min(written + rate * AUDIO_LEAD_SECONDS)
}

fn letterbox(source: (u32, u32), output: (u32, u32)) -> RECT {
    let (sw, sh) = (source.0.max(1) as u64, source.1.max(1) as u64);
    let (ow, oh) = (output.0 as u64, output.1 as u64);
    let (mut width, mut height) = if sw * oh > ow * sh {
        (ow, (ow * sh / sw).min(oh))
    } else {
        ((oh * sw / sh).min(ow), oh)
    };
    if ow - width <= 2 {
        width = ow;
    }
    if oh - height <= 2 {
        height = oh;
    }
    let left = (ow - width) / 2;
    let top = (oh - height) / 2;
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + width) as i32,
        bottom: (top + height) as i32,
    }
}

fn now_qpc_100ns() -> u64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let mut frequency = 0i64;
    let mut counter = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut frequency);
        let _ = QueryPerformanceCounter(&mut counter);
    }
    if frequency <= 0 {
        return 0;
    }
    (counter as i128 * 10_000_000 / frequency as i128) as u64
}

struct Discard(Option<PathBuf>);

impl Drop for Discard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            discard(&path);
        }
    }
}

type Converter = (
    ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessor,
    Vec<ID3D11VideoProcessorOutputView>,
);

fn crop_region(hwnd: i64, source: (u32, u32)) -> Region {
    let wanted = super::capture::client_region(HWND(hwnd as *mut core::ffi::c_void));
    super::capture::visible_region(wanted, source.0, source.1)
}

fn plan_frame(
    source: (u32, u32),
    client: Option<Region>,
    wanted_height: u32,
) -> (Region, (u32, u32)) {
    let region = super::capture::visible_region(client, source.0, source.1);
    (region, target_size((region.width, region.height), wanted_height))
}

unsafe fn build_converter(
    device: &ID3D11Device,
    video: &ID3D11VideoContext1,
    source: (u32, u32),
    region: Region,
    output: (u32, u32),
    nv12: &[ID3D11Texture2D],
) -> Result<Converter, String> {
    let vdev: ID3D11VideoDevice = device
        .cast()
        .map_err(|e| format!("this GPU cannot be used for recording: {e}"))?;
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputWidth: source.0,
        InputHeight: source.1,
        OutputWidth: output.0,
        OutputHeight: output.1,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        ..Default::default()
    };
    let enumerator = vdev
        .CreateVideoProcessorEnumerator(&content)
        .map_err(|e| format!("could not set up the video converter: {e}"))?;
    let processor = vdev
        .CreateVideoProcessor(&enumerator, 0)
        .map_err(|e| format!("could not set up the video converter: {e}"))?;

    video.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
    video.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
    video.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
    let black = D3D11_VIDEO_COLOR {
        Anonymous: D3D11_VIDEO_COLOR_0 {
            RGBA: D3D11_VIDEO_COLOR_RGBA {
                R: 0.0,
                G: 0.0,
                B: 0.0,
                A: 1.0,
            },
        },
    };
    video.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
    let src_rect = RECT {
        left: region.x as i32,
        top: region.y as i32,
        right: (region.x + region.width) as i32,
        bottom: (region.y + region.height) as i32,
    };
    let dst_rect = letterbox((region.width, region.height), output);
    video.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src_rect));
    video.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst_rect));

    let mut views = Vec::with_capacity(nv12.len());
    for texture in nv12 {
        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut view: Option<ID3D11VideoProcessorOutputView> = None;
        vdev.CreateVideoProcessorOutputView(texture, &enumerator, &view_desc, Some(&mut view))
            .map_err(|e| format!("could not allocate a video frame: {e}"))?;
        views.push(view.ok_or("Direct3D returned no output view")?);
    }
    Ok((enumerator, processor, views))
}

// ------------ Recording Session ------------
// Everything one clip needs while it records: the capture pool, the GPU converter, the
// encoder and the audio mixer. The frame loop and the finishing steps live in its impl.
struct Session {
    path: PathBuf,
    partial: PathBuf,
    thumb: PathBuf,
    thumb_done: bool,
    codec: String,
    device: ID3D11Device,
    winrt: IDirect3DDevice,
    context: ID3D11DeviceContext,
    video: ID3D11VideoContext1,
    vdev: ID3D11VideoDevice,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    nv12: Vec<ID3D11Texture2D>,
    views: Vec<ID3D11VideoProcessorOutputView>,
    item: GraphicsCaptureItem,
    closed: Arc<AtomicBool>,
    closed_token: Option<i64>,
    pool: Direct3D11CaptureFramePool,
    session: windows::Graphics::Capture::GraphicsCaptureSession,
    writer: IMFSinkWriter,
    video_stream: u32,
    audio_stream: Option<u32>,
    audio: Vec<audio::AudioCapture>,
    audio_rx: Option<Receiver<audio::Packet>>,
    fps: u32,
    hwnd: i64,
    source: (u32, u32),
    region: Region,
    region_checked: std::time::Instant,
    output: (u32, u32),
    slot: usize,
    origin: Option<u64>,
    last_slot: i64,
    last_tick: u64,
    audio_cap: Option<u64>,
    mixer: Option<audio::Mixer>,
    audio_written: u64,
    audio_names: Vec<String>,
    bitrate: u32,
    rate_control: &'static str,
    color_tagged: bool,
    warnings: Vec<String>,
    duplicated: u64,
    decimated: u64,
    skipped: u64,
    audio_failures: u64,
}

impl Session {
    fn begin(hwnd: i64, game_id: &str, settings: &RecSettings) -> Result<Self, String> {
        let raw = hwnd as *mut core::ffi::c_void;
        if super::overlay::is_exclusive_fullscreen(raw) {
            return Err(
                "The game is in exclusive fullscreen, which Windows cannot record. Switch it to Borderless or Windowed."
                    .to_string(),
            );
        }

        unsafe {
            let super::capture::Device {
                device,
                context,
                winrt,
            } = super::capture::create_device(
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            )?;
            if let Ok(mt) = context.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }

            let (item, size) = super::capture::window_item(HWND(raw), true)?;
            let source = (size.Width as u32, size.Height as u32);

            let client = super::capture::client_region(HWND(raw));
            let (region, (dst_w, dst_h)) = plan_frame(source, client, settings.height);

            let closed = Arc::new(AtomicBool::new(false));
            let flag = closed.clone();
            let handler = TypedEventHandler::<GraphicsCaptureItem, windows::core::IInspectable>::new(
                move |_, _| {
                    flag.store(true, Ordering::Release);
                    Ok(())
                },
            );
            let closed_token = match item.Closed(&handler) {
                Ok(token) => Some(token),
                Err(e) => {
                    log::debug!("recorder: cannot watch the game window for closing ({e})");
                    None
                }
            };

            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                RING as i32,
                size,
            )
            .map_err(|e| format!("could not create the capture pool: {e}"))?;
            let capture = pool
                .CreateCaptureSession(&item)
                .map_err(|e| format!("could not start recording: {e}"))?;
            let _ = capture.SetIsBorderRequired(false);
            let _ = capture.SetIsCursorCaptureEnabled(false);
            capture
                .StartCapture()
                .map_err(|e| format!("could not start recording: {e}"))?;

            let vctx: ID3D11VideoContext1 = context
                .cast()
                .map_err(|e| format!("this GPU cannot be used for recording: {e}"))?;
            let vdev: ID3D11VideoDevice = device
                .cast()
                .map_err(|e| format!("this GPU cannot be used for recording: {e}"))?;

            let mut nv12 = Vec::with_capacity(RING);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: dst_w,
                Height: dst_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            for _ in 0..RING {
                let mut texture: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
                    .map_err(|e| format!("could not allocate a video frame: {e}"))?;
                nv12.push(texture.ok_or("Direct3D returned no texture")?);
            }
            let (enumerator, processor, views) =
                build_converter(&device, &vctx, source, region, (dst_w, dst_h), &nv12)?;

            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            mf::create_dxgi_device_manager(&mut token, &mut manager)
                .map_err(|e| format!("could not share the GPU with the encoder: {e}"))?;
            let manager = manager.ok_or("Media Foundation returned no device manager")?;
            manager
                .ResetDevice(&device, token)
                .map_err(|e| format!("could not share the GPU with the encoder: {e}"))?;

            let directory = settings
                .directory
                .join(super::capture::game_folder(game_id));
            std::fs::create_dir_all(&directory).map_err(|e| {
                super::capture::save_error("Could not create the captures folder", &directory, &e)
            })?;
            sweep_partials(&directory, &settings.directory);
            let path = super::capture::capture_path(&directory, game_id, "mp4");

            let attrs = mf::create_attributes(4)
                .map_err(|e| format!("could not prepare the recorder: {e}"))?;
            attrs
                .SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)
                .ok();
            attrs
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .ok();
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager).ok();

            let wanted = match settings.codec.as_str() {
                "hevc" => "hevc",
                "av1" => "av1",
                _ => "h264",
            };
            let candidates: &[&str] = if wanted == "h264" {
                &["h264"]
            } else {
                &[wanted, "h264"]
            };
            let mut pending = Discard(None);
            let mut chosen = None;
            let mut last = String::new();
            let attempts = candidates
                .iter()
                .flat_map(|codec| [(*codec, true), (*codec, false)]);
            for (attempt, (codec, color)) in attempts.enumerate() {
                let partial = partial_path(&path, attempt);
                let wide = super::process_utils::wide_path(&partial);
                let writer = mf::create_sink_writer(&wide, &attrs).map_err(|e| {
                    if denied_by_windows(&e) {
                        super::capture::blocked_message(&directory)
                    } else {
                        format!("could not create the video file: {e}")
                    }
                })?;
                let bitrate = target_bitrate(dst_w, dst_h, settings.fps, codec, &settings.quality);
                match Self::add_video(&writer, codec, (dst_w, dst_h), settings.fps, bitrate, color)
                {
                    Ok((stream, tuned)) => {
                        pending.0 = Some(partial.clone());
                        chosen = Some((writer, stream, codec, bitrate, tuned, color, partial));
                        break;
                    }
                    Err(e) => {
                        if color {
                            log::debug!(
                                "recorder: the {codec} encoder refused the color description ({e}), trying without it"
                            );
                        } else {
                            log::warn!("recorder: this machine cannot encode {codec} ({e})");
                        }
                        last = e;
                        drop(writer);
                        discard(&partial);
                    }
                }
            }
            let Some((writer, video_stream, codec, bitrate, tuned, color_tagged, partial)) = chosen
            else {
                return Err(format!("This machine cannot encode video. {last}"));
            };
            if codec != wanted {
                log::info!(
                    "recorder: recording {codec} because this machine cannot encode {wanted}"
                );
            }
            let rate_control =
                configure_encoder(&writer, video_stream, bitrate, settings.fps, tuned);
            let codec = codec.to_string();

            let audio_names: Vec<String> = settings.audio.iter().map(|s| s.name.clone()).collect();
            let mut warnings = settings.warnings.clone();

            let (audio_stream, audio, audio_rx) = if settings.audio.is_empty() {
                (None, Vec::new(), None)
            } else {
                match Self::add_audio(&writer) {
                    Ok(stream) => {
                        let (tx, rx) = std::sync::mpsc::channel();
                        let mut started = Vec::new();
                        for (index, source) in settings.audio.iter().enumerate() {
                            match audio::start(
                                index,
                                source.kind,
                                source.name.clone(),
                                tx.clone(),
                            ) {
                                Ok(capture) => started.push(capture),
                                Err(e) => {
                                    log::warn!("recorder: {} audio: {e}", source.name);
                                    warnings.push(audio_warning(&source.name, &e));
                                }
                            }
                        }
                        let deadline = std::time::Instant::now() + AUDIO_READY_WAIT;
                        for capture in &started {
                            match capture.wait_ready(deadline) {
                                Some(Ok(())) => {}
                                Some(Err(e)) => warnings.push(audio_warning(capture.name(), &e)),
                                None => log::warn!(
                                    "recorder: {} audio had not started after {} ms",
                                    capture.name(),
                                    AUDIO_READY_WAIT.as_millis()
                                ),
                            }
                        }
                        if started.is_empty() {
                            warnings.push(
                                "None of the chosen audio sources could be opened, so this clip has no sound."
                                    .to_string(),
                            );
                            (None, Vec::new(), None)
                        } else {
                            (Some(stream), started, Some(rx))
                        }
                    }
                    Err(e) => {
                        log::warn!("recorder: no audio stream ({e})");
                        warnings.push(
                            "The audio track could not be added, so this clip has no sound."
                                .to_string(),
                        );
                        (None, Vec::new(), None)
                    }
                }
            };

            writer
                .BeginWriting()
                .map_err(|e| format!("could not start writing the video: {e}"))?;
            pending.0 = None;

            Ok(Self {
                thumb: super::capture::thumb_path(&settings.directory, &path),
                path,
                partial,
                thumb_done: false,
                device,
                winrt,
                context,
                video: vctx,
                vdev,
                processor,
                enumerator,
                nv12,
                views,
                item,
                closed,
                closed_token,
                pool,
                session: capture,
                writer,
                video_stream,
                audio_stream,
                audio,
                audio_rx,
                codec,
                fps: settings.fps,
                hwnd,
                source,
                region,
                region_checked: std::time::Instant::now(),
                output: (dst_w, dst_h),
                slot: 0,
                origin: None,
                last_slot: -1,
                last_tick: 0,
                audio_cap: None,
                mixer: None,
                audio_written: 0,
                audio_names,
                bitrate,
                rate_control,
                color_tagged,
                warnings,
                duplicated: 0,
                decimated: 0,
                skipped: 0,
                audio_failures: 0,
            })
        }
    }

    fn describe(&self) -> String {
        let audio = if self.audio_stream.is_some() {
            self.audio_names.join(", ")
        } else {
            "none".to_string()
        };
        format!(
            "{}, {}x{} game area of a {}x{} window scaled to {}x{} at {} fps, {} at {:.1} Mbps with {}, {}, audio [{}]",
            self.path.display(),
            self.region.width,
            self.region.height,
            self.source.0,
            self.source.1,
            self.output.0,
            self.output.1,
            self.fps,
            self.codec,
            self.bitrate as f64 / 1_000_000.0,
            self.rate_control,
            if self.color_tagged {
                "tagged BT.709"
            } else {
                "untagged color"
            },
            audio
        )
    }

    fn frame_stats(&self) -> String {
        format!(
            "{} frames captured, {} repeated to fill gaps, {} decimated, {} skipped, {} audio writes failed",
            self.slot, self.duplicated, self.decimated, self.skipped, self.audio_failures
        )
    }

    unsafe fn video_type(
        subtype: &windows::core::GUID,
        (width, height): (u32, u32),
        fps: u32,
        color: bool,
    ) -> Result<IMFMediaType, String> {
        let media = mf::create_media_type()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok();
        media.SetGUID(&MF_MT_SUBTYPE, subtype).ok();
        media.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height)).ok();
        media.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1)).ok();
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).ok();
        media
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .ok();
        if color {
            tag_bt709(&media);
        }
        Ok(media)
    }

    unsafe fn audio_type(
        subtype: &windows::core::GUID,
        bytes_per_second: u32,
    ) -> Result<IMFMediaType, String> {
        let media = mf::create_media_type()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ok();
        media.SetGUID(&MF_MT_SUBTYPE, subtype).ok();
        media
            .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio::SAMPLE_RATE)
            .ok();
        media
            .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, audio::CHANNELS)
            .ok();
        media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).ok();
        media
            .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_second)
            .ok();
        Ok(media)
    }

    unsafe fn add_video(
        writer: &IMFSinkWriter,
        codec: &str,
        size: (u32, u32),
        fps: u32,
        bitrate: u32,
        color: bool,
    ) -> Result<(u32, bool), String> {
        let out = Self::video_type(&subtype_for(codec), size, fps, color)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate).ok();
        let stream = writer.AddStream(&out).map_err(|e| e.to_string())?;

        let input = Self::video_type(&MFVideoFormat_NV12, size, fps, color)?;

        if let Some(params) = encoding_params(bitrate, fps) {
            match writer.SetInputMediaType(stream, &input, &params) {
                Ok(()) => return Ok((stream, true)),
                Err(e) => log::debug!(
                    "recorder: the {codec} encoder refused the rate control settings ({e})"
                ),
            }
        }
        writer
            .SetInputMediaType(stream, &input, None)
            .map_err(|e| e.to_string())?;
        Ok((stream, false))
    }

    unsafe fn add_audio(writer: &IMFSinkWriter) -> Result<u32, String> {
        let out = Self::audio_type(&MFAudioFormat_AAC, AUDIO_BYTES_PER_SECOND)?;
        out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).ok();
        let stream = writer
            .AddStream(&out)
            .map_err(|e| format!("could not add the audio stream: {e}"))?;

        let input = Self::audio_type(&MFAudioFormat_PCM, 192_000)?;
        input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4).ok();
        writer
            .SetInputMediaType(stream, &input, None)
            .map_err(|e| format!("could not add the audio stream: {e}"))?;
        Ok(stream)
    }

    fn pump(&mut self, rx: &Receiver<Command>) -> Result<Option<String>, String> {
        let interval = std::time::Duration::from_micros(1_000_000 / self.fps.max(1) as u64 / 2);
        self.last_tick = now_qpc_100ns();
        loop {
            match rx.try_recv() {
                Ok(Command::Toggle { .. }) | Ok(Command::Stop) | Ok(Command::Shutdown) => {
                    return Ok(None)
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(None),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }

            let now = now_qpc_100ns();
            let gap = now.saturating_sub(self.last_tick);
            if gap > SLEEP_GAP_100NS {
                self.audio_cap = self
                    .origin
                    .map(|origin| frames_since(self.last_tick, origin));
                log::warn!(
                    "recorder: stopped at a {}s clock gap (system sleep)",
                    gap / 10_000_000
                );
                return Ok(Some(
                    "Recording stopped because the PC went to sleep.".to_string(),
                ));
            }
            self.last_tick = now;

            if self.closed.load(Ordering::Acquire) {
                log::info!("recorder: the game window closed, ending the clip");
                return Ok(Some(
                    "Recording stopped because the game window closed.".to_string(),
                ));
            }

            self.drain_audio(false);

            match self.pool.TryGetNextFrame() {
                Ok(frame) => {
                    if let Ok(size) = frame.ContentSize() {
                        let seen = (size.Width.max(0) as u32, size.Height.max(0) as u32);
                        if seen != self.source {
                            let _ = frame.Close();
                            if seen.0 < 2 || seen.1 < 2 {
                                self.skipped += 1;
                                continue;
                            }
                            if let Err(e) = self.resize(seen) {
                                log::warn!(
                                    "recorder: could not follow the new window size ({e}), ending the clip"
                                );
                                return Ok(Some(
                                    "Recording stopped because the game changed resolution."
                                        .to_string(),
                                ));
                            }
                            continue;
                        }
                    }
                    if self.region_checked.elapsed() >= REGION_CHECK {
                        if let Err(e) = self.follow_region() {
                            let _ = frame.Close();
                            log::warn!(
                                "recorder: could not follow the new game area ({e}), ending the clip"
                            );
                            return Ok(Some(
                                "Recording stopped because the game changed resolution."
                                    .to_string(),
                            ));
                        }
                    }
                    self.write_frame(&frame)?;
                }
                Err(_) => std::thread::sleep(interval),
            }
        }
    }

    fn follow_region(&mut self) -> Result<(), String> {
        self.region_checked = std::time::Instant::now();
        let region = crop_region(self.hwnd, self.source);
        if region == self.region {
            return Ok(());
        }
        log::info!(
            "recorder: the game area changed from {}x{} to {}x{}, keeping the clip at {}x{}",
            self.region.width,
            self.region.height,
            region.width,
            region.height,
            self.output.0,
            self.output.1
        );
        self.convert_from(self.source, region)
    }

    fn convert_from(&mut self, source: (u32, u32), region: Region) -> Result<(), String> {
        let (enumerator, processor, views) = unsafe {
            build_converter(&self.device, &self.video, source, region, self.output, &self.nv12)?
        };
        self.enumerator = enumerator;
        self.processor = processor;
        self.views = views;
        self.source = source;
        self.region = region;
        Ok(())
    }

    fn resize(&mut self, size: (u32, u32)) -> Result<(), String> {
        log::info!(
            "recorder: the game window changed from {}x{} to {}x{}, keeping the clip at {}x{}",
            self.source.0,
            self.source.1,
            size.0,
            size.1,
            self.output.0,
            self.output.1
        );
        self.pool
            .Recreate(
                &self.winrt,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                RING as i32,
                SizeInt32 {
                    Width: size.0 as i32,
                    Height: size.1 as i32,
                },
            )
            .map_err(|e| format!("could not resize the capture pool: {e}"))?;
        self.region_checked = std::time::Instant::now();
        self.convert_from(size, crop_region(self.hwnd, size))
    }

    fn write_frame(
        &mut self,
        frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    ) -> Result<(), String> {
        unsafe {
            let stamp = frame
                .SystemRelativeTime()
                .map(|t| t.Duration as u64)
                .unwrap_or(0);
            let origin = *self.origin.get_or_insert(stamp);
            let elapsed = stamp.saturating_sub(origin);
            let slot = ((elapsed as i128 * self.fps as i128 + 5_000_000) / 10_000_000) as i64;
            if slot <= self.last_slot {
                self.decimated += 1;
                return Ok(());
            }

            let surface = frame
                .Surface()
                .map_err(|e| format!("the frame had no surface: {e}"))?;
            let access: IDirect3DDxgiInterfaceAccess = surface
                .cast()
                .map_err(|e| format!("could not reach the frame: {e}"))?;
            let bgra: ID3D11Texture2D = access
                .GetInterface()
                .map_err(|e| format!("could not reach the frame: {e}"))?;

            let mut surface_desc = D3D11_TEXTURE2D_DESC::default();
            bgra.GetDesc(&mut surface_desc);
            if surface_desc.Width < self.source.0 || surface_desc.Height < self.source.1 {
                self.skipped += 1;
                return Ok(());
            }

            if !self.thumb_done {
                self.thumb_done = true;
                self.snapshot_thumbnail(&bgra);
            }

            let index = self.slot % RING;
            let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut input = None;
            self.vdev
                .CreateVideoProcessorInputView(
                    &bgra,
                    &self.enumerator,
                    &view_desc,
                    Some(&mut input),
                )
                .map_err(|e| format!("could not convert the frame: {e}"))?;

            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(input),
                ..Default::default()
            }];
            let blt = self
                .video
                .VideoProcessorBlt(&self.processor, &self.views[index], 0, &streams);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            blt.map_err(|e| format!("could not convert the frame: {e}"))?;

            if self.last_slot >= 0 && slot > self.last_slot + 1 {
                let previous = (self.slot + RING - 1) % RING;
                let span = slot - self.last_slot - 1;
                self.submit(previous, self.last_slot + 1, span)?;
                self.duplicated += span as u64;
            }
            self.submit(index, slot, 1)?;
            self.last_slot = slot;
            self.slot += 1;
        }
        Ok(())
    }

    unsafe fn snapshot_thumbnail(&self, texture: &ID3D11Texture2D) {
        let Ok(frame) = super::capture::read_texture(
            &self.device,
            &self.context,
            texture,
            Some(self.region),
            None,
        ) else {
            return;
        };

        let dest = self.thumb.clone();
        std::thread::spawn(move || {
            let written = super::capture::frame_to_rgb(frame)
                .and_then(|img| super::capture::write_thumbnail(&img, &dest));
            if let Err(e) = written {
                log::debug!("recorder: no clip thumbnail: {e}");
            }
        });
    }

    unsafe fn submit(&self, index: usize, slot: i64, span: i64) -> Result<(), String> {
        let buffer: IMFMediaBuffer =
            mf::create_dxgi_surface_buffer(&ID3D11Texture2D::IID, &self.nv12[index], 0)
                .map_err(|e| format!("could not wrap the frame: {e}"))?;
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            if let Ok(len) = two_d.GetContiguousLength() {
                let _ = buffer.SetCurrentLength(len);
            }
        }

        let sample: IMFSample = mf::create_sample().map_err(|e| e.to_string())?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| format!("could not queue the frame: {e}"))?;
        let ticks = 10_000_000i64 / self.fps.max(1) as i64;
        sample.SetSampleTime(slot * ticks).ok();
        sample.SetSampleDuration(span.max(1) * ticks).ok();
        self.writer
            .WriteSample(self.video_stream, &sample)
            .map_err(|e| {
                if is_disk_full(&e) {
                    "the disk is full".to_string()
                } else {
                    format!("could not write the video: {e}")
                }
            })?;
        Ok(())
    }

    fn drain_audio(&mut self, flush: bool) {
        let (Some(stream), Some(origin)) = (self.audio_stream, self.origin) else {
            return;
        };
        let sources = self.audio_names.len();
        let mixer = self
            .mixer
            .get_or_insert_with(|| audio::Mixer::new(origin, sources));

        if let Some(rx) = self.audio_rx.as_ref() {
            while let Ok(packet) = rx.try_recv() {
                if !packet.pcm.is_empty() {
                    mixer.add(&packet);
                }
            }
        }

        let rate = audio::SAMPLE_RATE as u64;
        let elapsed = frames_since(now_qpc_100ns(), origin);
        let up_to = audio_target(elapsed, self.audio_written, flush, self.audio_cap);
        if !flush && up_to.saturating_sub(self.audio_written) < rate / 50 {
            return;
        }

        let pcm = mixer.take(up_to);
        if !pcm.is_empty() {
            self.write_audio(stream, &pcm);
        }
    }

    fn write_audio(&mut self, stream: u32, pcm: &[i16]) {
        let rate = audio::SAMPLE_RATE as u64;
        let channels = audio::CHANNELS as usize;
        for part in pcm.chunks(AUDIO_CHUNK_FRAMES * channels) {
            let frames = (part.len() / channels) as u64;
            let time = self.audio_written * 10_000_000 / rate;
            let duration = frames * 10_000_000 / rate;
            self.audio_written += frames;
            if let Err(e) = unsafe { self.write_audio_sample(stream, part, time, duration) } {
                self.audio_failures += 1;
                if self.audio_failures == 1 {
                    log::warn!(
                        "recorder: could not write audio ({e}), later failures are only counted"
                    );
                }
            }
        }
    }

    unsafe fn write_audio_sample(
        &self,
        stream: u32,
        pcm: &[i16],
        time: u64,
        duration: u64,
    ) -> Result<(), String> {
        let bytes: &[u8] =
            std::slice::from_raw_parts(pcm.as_ptr() as *const u8, std::mem::size_of_val(pcm));
        let len = u32::try_from(bytes.len()).map_err(|_| "the audio chunk is too large")?;
        let buffer = mf::create_memory_buffer(len)
            .map_err(|e| format!("no audio buffer: {e}"))?;
        let mut target: *mut u8 = std::ptr::null_mut();
        let mut max = 0u32;
        buffer
            .Lock(&mut target, Some(&mut max), None)
            .map_err(|e| format!("could not lock the audio buffer: {e}"))?;
        if target.is_null() || (max as usize) < bytes.len() {
            let _ = buffer.Unlock();
            return Err("the audio buffer is too small".to_string());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
        let _ = buffer.Unlock();
        let _ = buffer.SetCurrentLength(len);

        let sample = mf::create_sample().map_err(|e| format!("no audio sample: {e}"))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| format!("could not queue the audio: {e}"))?;
        sample.SetSampleTime(time as i64).ok();
        sample.SetSampleDuration(duration as i64).ok();
        self.writer
            .WriteSample(stream, &sample)
            .map_err(|e| format!("could not write the audio: {e}"))
    }

    fn finish(&mut self) -> Result<u64, String> {
        for source in self.audio.drain(..) {
            source.stop();
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.drain_audio(true);
        if let Some(mixer) = self.mixer.as_ref() {
            for (index, frames) in mixer.contributed().iter().enumerate() {
                let name = self
                    .audio_names
                    .get(index)
                    .map(String::as_str)
                    .unwrap_or("unknown");
                if *frames == 0 {
                    log::warn!("recorder: {name} contributed no audio to this clip");
                } else {
                    log::info!(
                        "recorder: {name} contributed {:.1}s of audio",
                        *frames as f64 / audio::SAMPLE_RATE as f64
                    );
                }
            }
        }
        let finalized = unsafe { self.writer.Finalize() };
        if let Some(token) = self.closed_token.take() {
            let _ = self.item.RemoveClosed(token);
        }
        let _ = self.session.Close();
        let _ = self.pool.Close();
        if self.last_slot < 0 {
            return Err("Nothing was recorded because the game did not draw a frame.".to_string());
        }
        if let Err(e) = finalized {
            return Err(if is_disk_full(&e) {
                "The clip could not be saved because the disk is full.".to_string()
            } else {
                format!("The clip could not be saved. {e}")
            });
        }
        Ok((self.last_slot as u64 + 1) * 1000 / self.fps.max(1) as u64)
    }
}

// ------------ Bitrate And Encoder Setup ------------
// Turns the quality setting, resolution and fps into a target bitrate, and configures
// the encoder for it. The overlay reuses the same numbers for its clip size estimate.
fn bits_per_pixel(quality: &str) -> f64 {
    match quality {
        "efficient" => 0.045,
        "high" => 0.140,
        _ => 0.085,
    }
}

const MODERN_CODEC_FACTOR: f64 = 0.62;
const MIN_BITRATE: u64 = 4_000_000;
const MAX_BITRATE: u64 = 60_000_000;
const AUDIO_BYTES_PER_SECOND: u32 = 16_000;

fn target_bitrate(width: u32, height: u32, fps: u32, codec: &str, quality: &str) -> u32 {
    let modern = matches!(codec, "hevc" | "av1");
    let per_pixel = bits_per_pixel(quality) * if modern { MODERN_CODEC_FACTOR } else { 1.0 };
    let raw = width as f64 * height as f64 * fps.max(1) as f64 * per_pixel;
    (raw as u64).clamp(MIN_BITRATE, MAX_BITRATE) as u32
}

fn peak_bitrate(bitrate: u32) -> u32 {
    bitrate.saturating_mul(2).min(80_000_000)
}

fn gop_size(fps: u32) -> u32 {
    fps.max(1) * 2
}

pub(super) fn bitrate_model() -> serde_json::Value {
    serde_json::json!({
        "bitsPerPixel": {
            "efficient": bits_per_pixel("efficient"),
            "balanced": bits_per_pixel("balanced"),
            "high": bits_per_pixel("high"),
        },
        "modernCodecFactor": MODERN_CODEC_FACTOR,
        "minBps": MIN_BITRATE,
        "maxBps": MAX_BITRATE,
        "audioBps": AUDIO_BYTES_PER_SECOND * 8,
    })
}

fn variant_u32(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    variant
}

fn encoding_params(bitrate: u32, fps: u32) -> Option<IMFAttributes> {
    let attrs = mf::create_attributes(4).ok()?;
    unsafe {
        attrs
            .SetUINT32(
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
            )
            .ok()?;
        attrs
            .SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, bitrate)
            .ok()?;
        attrs
            .SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, peak_bitrate(bitrate))
            .ok()?;
        attrs
            .SetUINT32(&CODECAPI_AVEncMPVGOPSize, gop_size(fps))
            .ok()?;
    }
    Some(attrs)
}

fn configure_encoder(
    writer: &IMFSinkWriter,
    stream: u32,
    bitrate: u32,
    fps: u32,
    tuned: bool,
) -> &'static str {
    let preset_mode = if tuned {
        "peak constrained VBR"
    } else {
        "the encoder's default rate control"
    };
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        if writer
            .GetServiceForStream(
                stream,
                &windows::core::GUID::zeroed(),
                &ICodecAPI::IID,
                &mut raw,
            )
            .is_err()
            || raw.is_null()
        {
            return preset_mode;
        }
        let api = ICodecAPI::from_raw(raw);

        let set = |name: &str, guid: &windows::core::GUID, value: u32| {
            let variant = variant_u32(value);
            match api.SetValue(guid, &variant) {
                Ok(()) => true,
                Err(e) => {
                    log::debug!("recorder: the encoder refused {name} ({e})");
                    false
                }
            }
        };

        let mode = if set(
            "peak constrained VBR",
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
        ) {
            "peak constrained VBR"
        } else if !tuned
            && set(
                "unconstrained VBR",
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32,
            )
        {
            "unconstrained VBR"
        } else {
            preset_mode
        };

        set("mean bitrate", &CODECAPI_AVEncCommonMeanBitRate, bitrate);
        set(
            "peak bitrate",
            &CODECAPI_AVEncCommonMaxBitRate,
            peak_bitrate(bitrate),
        );
        set("GOP size", &CODECAPI_AVEncMPVGOPSize, gop_size(fps));
        mode
    }
}

unsafe fn tag_bt709(media: &IMFMediaType) {
    media
        .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
        .ok();
    media
        .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
        .ok();
    media
        .SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
        .ok();
    media
        .SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
        .ok();
}

fn subtype_for(codec: &str) -> windows::core::GUID {
    match codec {
        "hevc" => MFVideoFormat_HEVC,
        "av1" => MFVideoFormat_AV1,
        _ => MFVideoFormat_H264,
    }
}

fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn target_size(source: (u32, u32), wanted_height: u32) -> (u32, u32) {
    let height = if wanted_height == 0 || wanted_height >= source.1 {
        source.1
    } else {
        wanted_height
    };
    let width = (height as u64 * source.0 as u64 / source.1.max(1) as u64) as u32;
    (width & !1, height & !1)
}

// ------------ Media Foundation Loader ------------
// Loads Media Foundation by hand so the launcher still starts on Windows setups that
// do not have it. Recording just reports itself as unavailable there.
mod mf {
    use super::*;
    use std::sync::OnceLock;
    use windows::core::{GUID, HRESULT, PCWSTR};
    use windows::Win32::Foundation::HMODULE as WHMODULE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
    type ShutdownFn = unsafe extern "system" fn() -> HRESULT;
    type CreateAttributesFn =
        unsafe extern "system" fn(*mut *mut core::ffi::c_void, u32) -> HRESULT;
    type CreateMediaTypeFn = unsafe extern "system" fn(*mut *mut core::ffi::c_void) -> HRESULT;
    type CreateSampleFn = unsafe extern "system" fn(*mut *mut core::ffi::c_void) -> HRESULT;
    type CreateMemoryBufferFn =
        unsafe extern "system" fn(u32, *mut *mut core::ffi::c_void) -> HRESULT;
    type CreateDxgiManagerFn =
        unsafe extern "system" fn(*mut u32, *mut *mut core::ffi::c_void) -> HRESULT;
    type CreateDxgiSurfaceBufferFn = unsafe extern "system" fn(
        *const GUID,
        *mut core::ffi::c_void,
        u32,
        i32,
        *mut *mut core::ffi::c_void,
    ) -> HRESULT;
    type CreateSinkWriterFn = unsafe extern "system" fn(
        PCWSTR,
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut *mut core::ffi::c_void,
    ) -> HRESULT;

    struct Api {
        startup: StartupFn,
        shutdown: ShutdownFn,
        create_attributes: CreateAttributesFn,
        create_media_type: CreateMediaTypeFn,
        create_sample: CreateSampleFn,
        create_memory_buffer: CreateMemoryBufferFn,
        create_dxgi_manager: CreateDxgiManagerFn,
        create_dxgi_surface_buffer: CreateDxgiSurfaceBufferFn,
        create_sink_writer: CreateSinkWriterFn,
    }

    unsafe impl Send for Api {}
    unsafe impl Sync for Api {}

    macro_rules! sym {
        ($module:expr, $name:literal, $ty:ty) => {
            std::mem::transmute::<*const core::ffi::c_void, $ty>(proc($module, $name)?)
        };
    }

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| unsafe {
            let plat = load("mfplat.dll")?;
            let rw = load("mfreadwrite.dll")?;
            Some(Api {
                startup: sym!(plat, b"MFStartup\0", StartupFn),
                shutdown: sym!(plat, b"MFShutdown\0", ShutdownFn),
                create_attributes: sym!(plat, b"MFCreateAttributes\0", CreateAttributesFn),
                create_media_type: sym!(plat, b"MFCreateMediaType\0", CreateMediaTypeFn),
                create_sample: sym!(plat, b"MFCreateSample\0", CreateSampleFn),
                create_memory_buffer: sym!(plat, b"MFCreateMemoryBuffer\0", CreateMemoryBufferFn),
                create_dxgi_manager: sym!(
                    plat,
                    b"MFCreateDXGIDeviceManager\0",
                    CreateDxgiManagerFn
                ),
                create_dxgi_surface_buffer: sym!(
                    plat,
                    b"MFCreateDXGISurfaceBuffer\0",
                    CreateDxgiSurfaceBufferFn
                ),
                create_sink_writer: sym!(rw, b"MFCreateSinkWriterFromURL\0", CreateSinkWriterFn),
            })
        })
        .as_ref()
    }

    unsafe fn load(name: &str) -> Option<WHMODULE> {
        let wide = peebify_helpers::common::wide(name);
        LoadLibraryW(PCWSTR(wide.as_ptr()))
            .ok()
            .filter(|m| !m.is_invalid())
    }

    unsafe fn proc(module: WHMODULE, name: &[u8]) -> Option<*const core::ffi::c_void> {
        GetProcAddress(module, windows::core::PCSTR(name.as_ptr())).map(|p| p as *const _)
    }

    pub fn ready() -> bool {
        api().is_some()
    }

    fn missing() -> String {
        "Recording is not available on this machine because Windows Media Foundation is missing."
            .to_string()
    }

    pub unsafe fn start_up() -> Result<(), String> {
        const MF_VERSION: u32 = 0x0002_0070;
        const MFSTARTUP_NOSOCKET: u32 = 1;
        let api = api().ok_or_else(missing)?;
        (api.startup)(MF_VERSION, MFSTARTUP_NOSOCKET)
            .ok()
            .map_err(|e| format!("{e}"))
    }

    pub unsafe fn shut_down() {
        if let Some(api) = api() {
            let _ = (api.shutdown)();
        }
    }

    unsafe fn out<T: windows::core::Interface>(
        call: impl FnOnce(&Api, *mut *mut core::ffi::c_void) -> HRESULT,
    ) -> Result<T, String> {
        let api = api().ok_or_else(missing)?;
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        call(api, &mut raw).ok().map_err(|e| e.to_string())?;
        if raw.is_null() {
            return Err("Media Foundation returned nothing".to_string());
        }
        Ok(T::from_raw(raw))
    }

    pub fn create_attributes(
        count: u32,
    ) -> Result<windows::Win32::Media::MediaFoundation::IMFAttributes, String> {
        unsafe { out(|api, p| (api.create_attributes)(p, count)) }
    }

    pub fn create_media_type() -> Result<IMFMediaType, String> {
        unsafe { out(|api, p| (api.create_media_type)(p)) }
    }

    pub fn create_sample() -> Result<IMFSample, String> {
        unsafe { out(|api, p| (api.create_sample)(p)) }
    }

    pub fn create_memory_buffer(len: u32) -> Result<IMFMediaBuffer, String> {
        unsafe { out(|api, p| (api.create_memory_buffer)(len, p)) }
    }

    pub fn create_dxgi_device_manager(
        token: &mut u32,
        manager: &mut Option<IMFDXGIDeviceManager>,
    ) -> Result<(), String> {
        *manager = Some(unsafe { out(|api, p| (api.create_dxgi_manager)(token, p))? });
        Ok(())
    }

    pub fn create_dxgi_surface_buffer(
        iid: &GUID,
        surface: &ID3D11Texture2D,
        subresource: u32,
    ) -> Result<IMFMediaBuffer, String> {
        let raw = surface.as_raw();
        unsafe { out(|api, p| (api.create_dxgi_surface_buffer)(iid, raw, subresource, 0, p)) }
    }

    pub fn create_sink_writer(
        path: &[u16],
        attrs: &windows::Win32::Media::MediaFoundation::IMFAttributes,
    ) -> Result<IMFSinkWriter, String> {
        let attrs_raw = attrs.as_raw();
        unsafe {
            out(|api, p| {
                (api.create_sink_writer)(PCWSTR(path.as_ptr()), std::ptr::null_mut(), attrs_raw, p)
            })
        }
    }
}

// ------------ Audio Capture ------------
// Captures sound for the clip recorder through WASAPI: the desktop mix, the microphone,
// or a single app's audio. Each source becomes 48 kHz stereo packets and the Mixer lines
// them up on the video clock. Only runs on Windows.
pub(crate) mod audio {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use windows::core::{Interface, Ref, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, eRender, ActivateAudioInterfaceAsync, EDataFlow,
        IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
        IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient, IMMDevice,
        IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
        AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
    use windows::Win32::System::Variant::VT_BLOB;

    pub const SAMPLE_RATE: u32 = 48_000;
    pub const CHANNELS: u32 = 2;

    const WAIT_MS: u32 = 100;
    const POLL_SLEEP: Duration = Duration::from_millis(4);
    const DEFAULT_CHECK: Duration = Duration::from_secs(1);
    const REOPEN_FIRST: Duration = Duration::from_millis(250);
    const REOPEN_MAX: Duration = Duration::from_secs(2);
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const EXTENSIBLE_EXTRA: u16 = 22;
    const SIDE_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;
    const LIMIT_KNEE: f32 = 0.8;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum SourceKind {
        Desktop,
        Microphone,
        Process(u32),
    }

    impl SourceKind {
        pub fn label(&self) -> String {
            match self {
                SourceKind::Desktop => "desktop".to_string(),
                SourceKind::Microphone => "microphone".to_string(),
                SourceKind::Process(pid) => format!("process {pid}"),
            }
        }
    }

    pub struct Packet {
        pub source: usize,
        pub pcm: Vec<i16>,
        pub qpc_100ns: u64,
    }

    pub struct AudioCapture {
        stop: Arc<AtomicBool>,
        name: String,
        ready: Receiver<Result<(), String>>,
    }

    impl AudioCapture {
        pub fn stop(&self) {
            self.stop.store(true, Ordering::Release);
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        pub fn wait_ready(&self, deadline: Instant) -> Option<Result<(), String>> {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.ready.recv_timeout(wait) {
                Ok(result) => Some(result),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    Some(Err("the audio thread ended before it started".to_string()))
                }
            }
        }
    }

    impl Drop for AudioCapture {
        fn drop(&mut self) {
            self.stop();
        }
    }

    pub fn start(
        index: usize,
        kind: SourceKind,
        name: String,
        tx: Sender<Packet>,
    ) -> Result<AudioCapture, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let thread_name = name.clone();

        std::thread::Builder::new()
            .name(format!("overlay-audio-{index}"))
            .spawn(move || {
                let name = thread_name;
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                match pump(index, kind, &tx, &thread_stop, &ready_tx) {
                    Ok(()) => log::info!("recorder: {name} audio ended"),
                    Err(e) => {
                        log::warn!("recorder: {name} audio stopped ({e})");
                        let _ = ready_tx.send(Err(e));
                    }
                }
                unsafe { windows::Win32::System::Com::CoUninitialize() };
            })
            .map_err(|e| format!("could not start the audio thread: {e}"))?;

        Ok(AudioCapture { stop, name, ready })
    }

    fn capture_error(loopback: u32, what: &str, e: &windows::core::Error) -> String {
        if loopback == 0 && e.code() == E_ACCESSDENIED {
            "Windows is blocking microphone access for desktop apps.".to_string()
        } else {
            format!("{what}: {e}")
        }
    }

    struct Event(HANDLE);

    impl Event {
        fn new(manual_reset: bool) -> Result<Self, String> {
            unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }
                .map(Event)
                .map_err(|e| format!("could not create the audio event: {e}"))
        }
    }

    unsafe impl Send for Event {}
    unsafe impl Sync for Event {}

    impl Drop for Event {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    #[windows_core::implement(IActivateAudioInterfaceCompletionHandler)]
    struct ActivationDone {
        signal: Arc<Event>,
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
        fn ActivateCompleted(
            &self,
            _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
        ) -> windows::core::Result<()> {
            unsafe { SetEvent(self.signal.0) }
        }
    }

    fn stereo_format(tag: u16, bits: u16) -> WAVEFORMATEX {
        let block_align = CHANNELS as u16 * bits / 8;
        WAVEFORMATEX {
            wFormatTag: tag,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: bits,
            cbSize: 0,
        }
    }

    unsafe fn process_loopback_client(pid: u32) -> Result<IAudioClient, String> {
        let signal = Arc::new(Event::new(true)?);

        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };

        let mut activation = std::mem::ManuallyDrop::new(PROPVARIANT::default());
        {
            let inner = &mut activation.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob.cbSize = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
            inner.Anonymous.blob.pBlobData = &mut params as *mut _ as *mut u8;
        }

        let handler = ActivationDone {
            signal: signal.clone(),
        };
        let handler: IActivateAudioInterfaceCompletionHandler = handler.into();

        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*activation),
            &handler,
        )
        .map_err(|e| format!("this build of Windows cannot capture one app's audio: {e}"))?;

        if WaitForSingleObject(signal.0, 5_000) != WAIT_OBJECT_0 {
            return Err("Windows did not answer the audio activation request.".to_string());
        }

        let mut status = windows::core::HRESULT(0);
        let mut activated: Option<windows::core::IUnknown> = None;
        operation
            .GetActivateResult(&mut status, &mut activated)
            .map_err(|e| format!("could not open that app's audio: {e}"))?;
        status
            .ok()
            .map_err(|e| format!("that app's audio could not be captured: {e}"))?;
        activated
            .ok_or_else(|| "Windows returned no audio client for that app.".to_string())?
            .cast::<IAudioClient>()
            .map_err(|e| format!("could not open that app's audio: {e}"))
    }

    fn flow_of(kind: SourceKind) -> EDataFlow {
        if matches!(kind, SourceKind::Microphone) {
            eCapture
        } else {
            eRender
        }
    }

    unsafe fn device_id(device: &IMMDevice) -> Option<String> {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        text
    }

    unsafe fn default_device_id(enumerator: &IMMDeviceEnumerator, kind: SourceKind) -> Option<String> {
        let device = enumerator
            .GetDefaultAudioEndpoint(flow_of(kind), eConsole)
            .ok()?;
        device_id(&device)
    }

    unsafe fn converted_client(device: &IMMDevice, loopback: u32) -> Result<IAudioClient, String> {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| capture_error(loopback, "could not open the audio device", &e))?;
        let format = stereo_format(WAVE_FORMAT_IEEE_FLOAT, 32);
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                loopback | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                10_000_000,
                0,
                &format,
                None,
            )
            .map_err(|e| capture_error(loopback, "could not start capturing", &e))?;
        Ok(client)
    }

    unsafe fn native_client(device: &IMMDevice, loopback: u32) -> Result<(IAudioClient, Source), String> {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| capture_error(loopback, "could not open the audio device", &e))?;
        let mix = client
            .GetMixFormat()
            .map_err(|e| format!("could not read the audio format: {e}"))?;
        let source = describe(mix);
        let result = client.Initialize(AUDCLNT_SHAREMODE_SHARED, loopback, 10_000_000, 0, mix, None);
        CoTaskMemFree(Some(mix as *const _));
        result.map_err(|e| capture_error(loopback, "could not start capturing", &e))?;
        Ok((client, source))
    }

    unsafe fn endpoint_client(
        device: &IMMDevice,
        kind: SourceKind,
    ) -> Result<(IAudioClient, Source), String> {
        let loopback = if matches!(kind, SourceKind::Microphone) {
            0
        } else {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        };
        match converted_client(device, loopback) {
            Ok(client) => Ok((client, Source::stereo(true))),
            Err(e) => {
                log::info!(
                    "recorder: Windows would not convert {} audio to stereo ({e}), so it is mixed down here",
                    kind.label()
                );
                native_client(device, loopback)
            }
        }
    }

    struct Stream {
        client: IAudioClient,
        capture: IAudioCaptureClient,
        source: Source,
        watch: Option<(IMMDeviceEnumerator, String)>,
        signal: Option<Event>,
    }

    unsafe fn open_stream(kind: SourceKind) -> Result<Stream, String> {
        let (client, source, watch, signal) = match kind {
            SourceKind::Process(pid) => {
                let client = process_loopback_client(pid)?;
                let format = stereo_format(WAVE_FORMAT_PCM as u16, 16);
                let signal = Event::new(false)?;
                client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                        0,
                        0,
                        &format,
                        None,
                    )
                    .map_err(|e| format!("could not start that app's audio: {e}"))?;
                client
                    .SetEventHandle(signal.0)
                    .map_err(|e| format!("could not arm that app's audio: {e}"))?;
                (client, Source::stereo(false), None, Some(signal))
            }
            _ => {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                        .map_err(|e| format!("no audio device enumerator: {e}"))?;
                let device = enumerator
                    .GetDefaultAudioEndpoint(flow_of(kind), eConsole)
                    .map_err(|e| format!("no default audio device: {e}"))?;
                let id = device_id(&device);
                let (client, source) = endpoint_client(&device, kind)?;
                (client, source, id.map(|id| (enumerator, id)), None)
            }
        };

        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| format!("could not get the capture client: {e}"))?;
        client
            .Start()
            .map_err(|e| format!("could not start capturing: {e}"))?;

        Ok(Stream {
            client,
            capture,
            source,
            watch,
            signal,
        })
    }

    enum Outcome {
        Stopped,
        Closed,
        Moved,
        Lost(String),
    }

    unsafe fn run(
        stream: &Stream,
        kind: SourceKind,
        index: usize,
        tx: &Sender<Packet>,
        stop: &AtomicBool,
        delivered: &mut u64,
    ) -> Outcome {
        let mut resampler = Resampler::new(stream.source.rate, SAMPLE_RATE);
        let mut checked = Instant::now();

        while !stop.load(Ordering::Acquire) {
            if let Some((enumerator, id)) = &stream.watch {
                if checked.elapsed() >= DEFAULT_CHECK {
                    checked = Instant::now();
                    if default_device_id(enumerator, kind).is_some_and(|current| &current != id) {
                        return Outcome::Moved;
                    }
                }
            }

            match &stream.signal {
                Some(signal) => {
                    if WaitForSingleObject(signal.0, WAIT_MS) != WAIT_OBJECT_0 {
                        if let Err(e) = stream.capture.GetNextPacketSize() {
                            return Outcome::Lost(e.to_string());
                        }
                        continue;
                    }
                }
                None => match stream.capture.GetNextPacketSize() {
                    Ok(0) => {
                        std::thread::sleep(POLL_SLEEP);
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => return Outcome::Lost(e.to_string()),
                },
            }

            loop {
                match stream.capture.GetNextPacketSize() {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => return Outcome::Lost(e.to_string()),
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut position = 0u64;
                if let Err(e) = stream.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut position),
                ) {
                    return Outcome::Lost(e.to_string());
                }

                if frames > 0 {
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    let stereo = if silent {
                        vec![0i16; frames as usize * CHANNELS as usize]
                    } else {
                        to_stereo_i16(data, frames, &stream.source)
                    };
                    let pcm = resampler.push(&stereo);
                    if !pcm.is_empty() {
                        *delivered += 1;
                        if tx
                            .send(Packet {
                                source: index,
                                pcm,
                                qpc_100ns: position,
                            })
                            .is_err()
                        {
                            let _ = stream.capture.ReleaseBuffer(frames);
                            return Outcome::Closed;
                        }
                    }
                }

                let _ = stream.capture.ReleaseBuffer(frames);
            }
        }
        Outcome::Stopped
    }

    fn wait_unless_stopped(stop: &AtomicBool, duration: Duration) -> bool {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            if stop.load(Ordering::Acquire) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !stop.load(Ordering::Acquire)
    }

    unsafe fn reopen(kind: SourceKind, stop: &AtomicBool) -> Option<Stream> {
        let mut delay = REOPEN_FIRST;
        let mut reported = false;
        loop {
            if !wait_unless_stopped(stop, delay) {
                return None;
            }
            match open_stream(kind) {
                Ok(stream) => return Some(stream),
                Err(e) => {
                    if !reported {
                        reported = true;
                        log::warn!(
                            "recorder: {} audio could not be reopened yet ({e}), still trying",
                            kind.label()
                        );
                    }
                    delay = (delay * 2).min(REOPEN_MAX);
                }
            }
        }
    }

    fn pump(
        index: usize,
        kind: SourceKind,
        tx: &Sender<Packet>,
        stop: &AtomicBool,
        ready: &Sender<Result<(), String>>,
    ) -> Result<(), String> {
        unsafe {
            let mut stream = open_stream(kind)?;
            let _ = ready.send(Ok(()));
            let mut delivered = 0u64;

            loop {
                let outcome = run(&stream, kind, index, tx, stop, &mut delivered);
                let _ = stream.client.Stop();
                match outcome {
                    Outcome::Stopped => break,
                    Outcome::Closed => return Ok(()),
                    Outcome::Lost(reason) if matches!(kind, SourceKind::Process(_)) => {
                        return Err(format!("the app's audio stream went away: {reason}"));
                    }
                    Outcome::Lost(reason) => log::warn!(
                        "recorder: {} audio device went away ({reason}), reopening",
                        kind.label()
                    ),
                    Outcome::Moved => log::info!(
                        "recorder: the default {} device changed, following it",
                        kind.label()
                    ),
                }
                drop(stream);
                match reopen(kind, stop) {
                    Some(next) => {
                        log::info!("recorder: {} audio reopened", kind.label());
                        stream = next;
                    }
                    None => break,
                }
            }

            if delivered == 0 {
                log::warn!(
                    "recorder: {} produced no audio at all during this recording",
                    kind.label()
                );
            }
        }
        Ok(())
    }

    struct Source {
        rate: u32,
        channels: u32,
        float: bool,
        bytes: u16,
        gains: Vec<(f32, f32)>,
        limit: bool,
    }

    impl Source {
        fn stereo(float: bool) -> Self {
            Source {
                rate: SAMPLE_RATE,
                channels: CHANNELS,
                float,
                bytes: if float { 4 } else { 2 },
                gains: channel_gains(0, CHANNELS as usize),
                limit: false,
            }
        }
    }

    fn default_mask(channels: usize) -> u32 {
        match channels {
            3 => 0x7,
            4 => 0x33,
            5 => 0x37,
            6 => 0x3F,
            7 => 0x70F,
            8 => 0x63F,
            _ => 0x3,
        }
    }

    fn speaker_gain(bit: u32) -> (f32, f32) {
        const FRONT_LEFT: u32 = 0x1 | 0x40;
        const FRONT_RIGHT: u32 = 0x2 | 0x80;
        const REAR_LEFT: u32 = 0x10 | 0x200 | 0x1000 | 0x8000;
        const REAR_RIGHT: u32 = 0x20 | 0x400 | 0x4000 | 0x20000;
        const CENTER: u32 = 0x4 | 0x100 | 0x800 | 0x2000 | 0x10000;
        if bit & FRONT_LEFT != 0 {
            (1.0, 0.0)
        } else if bit & FRONT_RIGHT != 0 {
            (0.0, 1.0)
        } else if bit & REAR_LEFT != 0 {
            (SIDE_GAIN, 0.0)
        } else if bit & REAR_RIGHT != 0 {
            (0.0, SIDE_GAIN)
        } else if bit & CENTER != 0 {
            (SIDE_GAIN, SIDE_GAIN)
        } else {
            (0.0, 0.0)
        }
    }

    fn channel_gains(mask: u32, channels: usize) -> Vec<(f32, f32)> {
        if channels <= 1 {
            return vec![(1.0, 1.0)];
        }
        let mask = if mask.count_ones() as usize >= channels {
            mask
        } else {
            default_mask(channels)
        };
        let mut bits = (0..32u32)
            .map(|shift| 1u32 << shift)
            .filter(|bit| mask & bit != 0);
        (0..channels)
            .map(|_| bits.next().map(speaker_gain).unwrap_or((0.0, 0.0)))
            .collect()
    }

    fn describe(format: *const WAVEFORMATEX) -> Source {
        let header = unsafe { std::ptr::read_unaligned(format) };
        let channels = header.nChannels.max(1);
        let extensible =
            header.wFormatTag == WAVE_FORMAT_EXTENSIBLE && header.cbSize >= EXTENSIBLE_EXTRA;
        let (float, mask) = if extensible {
            let extended = unsafe { std::ptr::read_unaligned(format as *const WAVEFORMATEXTENSIBLE) };
            let sub = extended.SubFormat;
            let mask = extended.dwChannelMask;
            (sub.data1 == WAVE_FORMAT_IEEE_FLOAT as u32, mask)
        } else {
            (header.wFormatTag == WAVE_FORMAT_IEEE_FLOAT, 0)
        };
        let bytes = if header.nBlockAlign >= channels {
            header.nBlockAlign / channels
        } else {
            header.wBitsPerSample / 8
        };
        Source {
            rate: header.nSamplesPerSec,
            channels: channels as u32,
            float,
            bytes,
            gains: channel_gains(mask, channels as usize),
            limit: channels > 2,
        }
    }

    fn to_stereo_i16(data: *const u8, frames: u32, source: &Source) -> Vec<i16> {
        let channels = source.channels.max(1) as usize;
        let mut out = Vec::with_capacity(frames as usize * CHANNELS as usize);

        for frame in 0..frames as usize {
            let mut left = 0f32;
            let mut right = 0f32;
            for channel in 0..channels {
                let index = frame * channels + channel;
                let value = unsafe { sample_at(data, index, source) };
                let (to_left, to_right) = source.gains.get(channel).copied().unwrap_or((0.0, 0.0));
                left += value * to_left;
                right += value * to_right;
            }
            if source.limit {
                left = soft_limit(left);
                right = soft_limit(right);
            }
            out.push(clamp_i16(left));
            out.push(clamp_i16(right));
        }
        out
    }

    unsafe fn sample_at(data: *const u8, index: usize, source: &Source) -> f32 {
        let at = data.add(index * source.bytes as usize);
        match (source.float, source.bytes) {
            (true, 4) => std::ptr::read_unaligned(at as *const f32),
            (true, 8) => std::ptr::read_unaligned(at as *const f64) as f32,
            (false, 1) => (*at as f32 - 128.0) / 128.0,
            (false, 2) => std::ptr::read_unaligned(at as *const i16) as f32 / 32768.0,
            (false, 3) => {
                let raw = i32::from_le_bytes([0, *at, *at.add(1), *at.add(2)]);
                raw as f32 / 2_147_483_648.0
            }
            (false, 4) => std::ptr::read_unaligned(at as *const i32) as f32 / 2_147_483_648.0,
            _ => 0.0,
        }
    }

    fn soft_limit(value: f32) -> f32 {
        let magnitude = value.abs();
        if magnitude <= LIMIT_KNEE {
            return value;
        }
        let over = (magnitude - LIMIT_KNEE) / (1.0 - LIMIT_KNEE);
        (LIMIT_KNEE + (1.0 - LIMIT_KNEE) * over / (1.0 + over)).copysign(value)
    }

    fn clamp_i16(value: f32) -> i16 {
        (value.clamp(-1.0, 1.0) * 32767.0) as i16
    }

    struct Resampler {
        from: u32,
        to: u32,
        position: f64,
        tail: [i16; 2],
        primed: bool,
    }

    impl Resampler {
        fn new(from: u32, to: u32) -> Self {
            Self {
                from: from.max(1),
                to,
                position: 0.0,
                tail: [0, 0],
                primed: false,
            }
        }

        fn push(&mut self, input: &[i16]) -> Vec<i16> {
            if self.from == self.to {
                return input.to_vec();
            }
            let frames = input.len() / 2;
            if frames == 0 {
                return Vec::new();
            }
            let step = self.from as f64 / self.to as f64;
            let mut out = Vec::with_capacity((frames as f64 / step) as usize * 2 + 4);

            while self.position < frames as f64 {
                let index = self.position as usize;
                let frac = self.position - index as f64;
                for channel in 0..2 {
                    let previous = if index == 0 {
                        if self.primed {
                            self.tail[channel]
                        } else {
                            input[channel]
                        }
                    } else {
                        input[(index - 1) * 2 + channel]
                    };
                    let current = input[index * 2 + channel];
                    let value = previous as f64 + (current as f64 - previous as f64) * frac;
                    out.push(value as i16);
                }
                self.position += step;
            }

            self.position -= frames as f64;
            self.tail = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]];
            self.primed = true;
            out
        }
    }

    const CURSOR_TOLERANCE_FRAMES: u64 = (SAMPLE_RATE / 50) as u64;

    pub struct Mixer {
        origin: u64,
        base: u64,
        accumulator: Vec<i32>,
        horizon: usize,
        contributed: Vec<u64>,
        cursors: Vec<Option<u64>>,
    }

    impl Mixer {
        pub fn new(origin: u64, sources: usize) -> Self {
            Self {
                origin,
                base: 0,
                accumulator: Vec::new(),
                horizon: (SAMPLE_RATE * 5) as usize,
                contributed: vec![0; sources],
                cursors: vec![None; sources],
            }
        }

        pub fn contributed(&self) -> &[u64] {
            &self.contributed
        }

        pub fn add(&mut self, packet: &Packet) {
            let Some(offset) = packet.qpc_100ns.checked_sub(self.origin) else {
                return;
            };
            let clock_start = offset * SAMPLE_RATE as u64 / 10_000_000;
            let frames = packet.pcm.len() / CHANNELS as usize;
            if frames == 0 {
                return;
            }
            let cursor = self.cursors.get(packet.source).copied().flatten();
            let start = match cursor {
                Some(next) if next.abs_diff(clock_start) <= CURSOR_TOLERANCE_FRAMES => next,
                _ => clock_start,
            };
            if let Some(slot) = self.cursors.get_mut(packet.source) {
                *slot = Some(start + frames as u64);
            }

            let (start, skip) = if start < self.base {
                let behind = (self.base - start) as usize;
                if behind >= frames {
                    return;
                }
                (self.base, behind)
            } else {
                (start, 0)
            };

            let at = (start - self.base) as usize;
            if at > self.horizon {
                return;
            }
            let needed = (at + (frames - skip)) * CHANNELS as usize;
            if self.accumulator.len() < needed {
                self.accumulator.resize(needed, 0);
            }
            for frame in skip..frames {
                for channel in 0..CHANNELS as usize {
                    let target = (at + frame - skip) * CHANNELS as usize + channel;
                    self.accumulator[target] += packet.pcm[frame * CHANNELS as usize + channel] as i32;
                }
            }
            if let Some(count) = self.contributed.get_mut(packet.source) {
                *count += (frames - skip) as u64;
            }
        }

        pub fn take(&mut self, up_to: u64) -> Vec<i16> {
            if up_to <= self.base {
                return Vec::new();
            }
            let frames = (up_to - self.base) as usize;
            let samples = frames * CHANNELS as usize;
            if self.accumulator.len() < samples {
                self.accumulator.resize(samples, 0);
            }
            let out: Vec<i16> = self
                .accumulator
                .drain(..samples)
                .map(|v| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                .collect();
            self.base = up_to;
            out
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn packet(source: usize, qpc: u64, first: i16, frames: usize) -> Packet {
            let mut pcm = Vec::with_capacity(frames * 2);
            for i in 0..frames {
                let v = first.wrapping_add(i as i16);
                pcm.push(v);
                pcm.push(v);
            }
            Packet {
                source,
                pcm,
                qpc_100ns: qpc,
            }
        }

        #[test]
        fn jittered_packets_stay_contiguous() {
            let origin = 1_000_000;
            let mut mixer = Mixer::new(origin, 1);
            let jitter = [0i64, 37, -41, 12, -8, 55, -60, 3, 29, -17];
            for i in 0..200usize {
                let nominal = origin + i as u64 * 100_000;
                let qpc = (nominal as i64 + jitter[i % jitter.len()]) as u64;
                mixer.add(&packet(0, qpc, (i * 480) as i16, 480));
            }
            let out = mixer.take(200 * 480);
            assert_eq!(out.len(), 200 * 480 * 2);
            for (frame, chunk) in out.chunks(2).enumerate() {
                assert_eq!(chunk[0], frame as i16, "frame {frame} left");
                assert_eq!(chunk[1], frame as i16, "frame {frame} right");
            }
        }

        #[test]
        fn a_real_gap_is_filled_with_silence() {
            let origin = 0;
            let mut mixer = Mixer::new(origin, 1);
            mixer.add(&packet(0, 0, 1000, 480));
            mixer.add(&packet(0, 1_000_000, 2000, 480));
            let out = mixer.take(4800 + 480);
            assert_eq!(out[0], 1000);
            assert_eq!(out[479 * 2], 1000 + 479);
            assert!(out[480 * 2..4800 * 2].iter().all(|v| *v == 0));
            assert_eq!(out[4800 * 2], 2000);
        }

        #[test]
        fn sources_keep_independent_cursors_and_sum() {
            let mut mixer = Mixer::new(0, 2);
            mixer.add(&packet(0, 0, 100, 480));
            mixer.add(&packet(1, 25, 5, 480));
            mixer.add(&packet(0, 100_000 - 30, 580, 480));
            mixer.add(&packet(1, 100_000 + 40, 485, 480));
            let out = mixer.take(960);
            for frame in 0..960usize {
                assert_eq!(
                    out[frame * 2],
                    (100 + frame) as i16 + (5 + frame) as i16,
                    "frame {frame}"
                );
            }
        }

        #[test]
        fn resampler_passthrough_is_identity() {
            let mut r = Resampler::new(48_000, 48_000);
            let input: Vec<i16> = (0..960).collect();
            assert_eq!(r.push(&input), input);
        }

        fn float_frames(samples: &[f32]) -> Vec<u8> {
            samples.iter().flat_map(|s| s.to_le_bytes()).collect()
        }

        fn surround_source(mask: u32) -> Source {
            Source {
                rate: SAMPLE_RATE,
                channels: 8,
                float: true,
                bytes: 4,
                gains: channel_gains(mask, 8),
                limit: true,
            }
        }

        #[test]
        fn seven_one_keeps_sides_apart_and_drops_lfe() {
            let gains = channel_gains(0x63F, 8);
            assert_eq!(gains[0], (1.0, 0.0));
            assert_eq!(gains[1], (0.0, 1.0));
            assert_eq!(gains[2], (SIDE_GAIN, SIDE_GAIN));
            assert_eq!(gains[3], (0.0, 0.0));
            assert_eq!(gains[4], (SIDE_GAIN, 0.0));
            assert_eq!(gains[5], (0.0, SIDE_GAIN));
            assert_eq!(gains[6], (SIDE_GAIN, 0.0));
            assert_eq!(gains[7], (0.0, SIDE_GAIN));
        }

        #[test]
        fn missing_mask_falls_back_to_the_standard_layout() {
            assert_eq!(channel_gains(0, 8), channel_gains(0x63F, 8));
            assert_eq!(channel_gains(0, 6), channel_gains(0x3F, 6));
            assert_eq!(channel_gains(0, 1), vec![(1.0, 1.0)]);
        }

        #[test]
        fn left_surround_stays_on_the_left() {
            let source = surround_source(0x63F);
            let data = float_frames(&[0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0]);
            let out = to_stereo_i16(data.as_ptr(), 1, &source);
            assert!(out[0] > 0);
            assert_eq!(out[1], 0);
        }

        #[test]
        fn front_only_content_keeps_its_level() {
            let source = surround_source(0x63F);
            let data = float_frames(&[0.5, 0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
            let out = to_stereo_i16(data.as_ptr(), 1, &source);
            assert_eq!(out[0], clamp_i16(0.5));
            assert_eq!(out[1], clamp_i16(0.25));
        }

        #[test]
        fn loud_surround_is_limited_not_clipped() {
            let source = surround_source(0x63F);
            let data = float_frames(&[1.0; 8]);
            let out = to_stereo_i16(data.as_ptr(), 1, &source);
            assert!(out[0] < i16::MAX);
            assert!(out[0] > clamp_i16(LIMIT_KNEE));
        }

        #[test]
        fn soft_limit_is_transparent_below_the_knee_and_bounded_above() {
            assert_eq!(soft_limit(0.5), 0.5);
            assert_eq!(soft_limit(-0.5), -0.5);
            assert!(soft_limit(3.0) < 1.0);
            assert!(soft_limit(-3.0) > -1.0);
            assert!(soft_limit(0.9) < soft_limit(1.2));
        }

        #[test]
        fn packed_24_bit_samples_decode() {
            let source = Source {
                rate: SAMPLE_RATE,
                channels: 2,
                float: false,
                bytes: 3,
                gains: channel_gains(0x3, 2),
                limit: false,
            };
            let data = [0x00u8, 0x00, 0x40, 0x00, 0x00, 0xC0];
            let out = to_stereo_i16(data.as_ptr(), 1, &source);
            assert_eq!(out[0], clamp_i16(0.5));
            assert_eq!(out[1], clamp_i16(-0.5));
        }

        fn extensible(bits: u16, channels: u16, mask: u32, sub: u32) -> WAVEFORMATEXTENSIBLE {
            let mut format = WAVEFORMATEXTENSIBLE::default();
            format.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE;
            format.Format.nChannels = channels;
            format.Format.nSamplesPerSec = 44_100;
            format.Format.wBitsPerSample = bits;
            format.Format.nBlockAlign = channels * bits / 8;
            format.Format.cbSize = EXTENSIBLE_EXTRA;
            format.dwChannelMask = mask;
            format.SubFormat = windows::core::GUID::from_values(
                sub,
                0,
                0x10,
                [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
            );
            format
        }

        #[test]
        fn describe_reads_the_extensible_subformat() {
            let float = extensible(32, 8, 0x63F, 3);
            let source = describe(&float as *const _ as *const WAVEFORMATEX);
            assert!(source.float);
            assert_eq!(source.bytes, 4);
            assert_eq!(source.channels, 8);
            assert_eq!(source.rate, 44_100);
            assert!(source.limit);
            assert_eq!(source.gains, channel_gains(0x63F, 8));

            let integer = extensible(32, 2, 0x3, 1);
            let source = describe(&integer as *const _ as *const WAVEFORMATEX);
            assert!(!source.float);
            assert_eq!(source.bytes, 4);
            assert!(!source.limit);

            let packed = extensible(24, 2, 0x3, 1);
            let source = describe(&packed as *const _ as *const WAVEFORMATEX);
            assert!(!source.float);
            assert_eq!(source.bytes, 3);
        }
    }
}

// ------------ Tests ------------
// Covers the bitrate maths, letterboxing, toggle handling and the partial clip recovery.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_model_matches_target_bitrate() {
        let model = bitrate_model();
        let bpp = model["bitsPerPixel"]["balanced"].as_f64().unwrap();
        let factor = model["modernCodecFactor"].as_f64().unwrap();
        let expected = (2560.0 * 1440.0 * 60.0 * bpp) as u32;
        assert_eq!(target_bitrate(2560, 1440, 60, "h264", "balanced"), expected);
        let modern = (2560.0 * 1440.0 * 60.0 * bpp * factor) as u32;
        assert_eq!(target_bitrate(2560, 1440, 60, "hevc", "balanced"), modern);
        assert_eq!(
            target_bitrate(640, 360, 30, "h264", "efficient") as u64,
            model["minBps"].as_u64().unwrap()
        );
        assert_eq!(
            target_bitrate(7680, 4320, 120, "h264", "high") as u64,
            model["maxBps"].as_u64().unwrap()
        );
        assert_eq!(model["audioBps"].as_u64(), Some(128_000));
    }

    #[test]
    fn letterbox_fills_matching_aspect_and_bars_the_rest() {
        let rect = |r: RECT| (r.left, r.top, r.right, r.bottom);
        assert_eq!(rect(letterbox((2560, 1440), (1920, 1080))), (0, 0, 1920, 1080));
        assert_eq!(rect(letterbox((1001, 601), (1000, 600))), (0, 0, 1000, 600));
        assert_eq!(rect(letterbox((1080, 1080), (1920, 1080))), (420, 0, 1500, 1080));
        assert_eq!(rect(letterbox((3840, 1080), (1920, 1080))), (0, 270, 1920, 810));
    }

    #[test]
    fn clips_crop_to_the_game_area_before_scaling() {
        let region = |x, y, width, height| Region {
            x,
            y,
            width,
            height,
        };
        let windowed = Some(region(8, 31, 1280, 720));
        assert_eq!(plan_frame((1296, 759), windowed, 480), (region(8, 31, 1280, 720), (852, 480)));
        let borderless = Some(region(0, 0, 2560, 1440));
        assert_eq!(plan_frame((2560, 1440), borderless, 1080), (region(0, 0, 2560, 1440), (1920, 1080)));
        assert_eq!(
            plan_frame((1296, 759), Some(region(8, 31, 1920, 1080)), 0),
            (region(8, 31, 1288, 728), (1288, 728))
        );
        assert_eq!(plan_frame((1920, 1080), None, 720), (region(0, 0, 1920, 1080), (1280, 720)));
    }

    fn toggle() -> Command {
        Command::Toggle {
            hwnd: 0,
            game_id: String::new(),
            settings: RecSettings {
                height: 0,
                fps: 60,
                quality: String::new(),
                directory: PathBuf::new(),
                audio: Vec::new(),
                warnings: Vec::new(),
                codec: String::new(),
            },
        }
    }

    #[test]
    fn toggles_while_starting_cancel_instead_of_replaying() {
        let (tx, rx) = std::sync::mpsc::channel();
        assert_eq!(pending_while_starting(&rx), (false, false));

        tx.send(toggle()).unwrap();
        tx.send(toggle()).unwrap();
        assert_eq!(pending_while_starting(&rx), (true, false));
        assert!(rx.try_recv().is_err(), "nothing is left to replay");

        tx.send(Command::Stop).unwrap();
        assert_eq!(pending_while_starting(&rx), (true, false));

        tx.send(Command::Shutdown).unwrap();
        assert_eq!(pending_while_starting(&rx), (false, true));

        drop(tx);
        assert_eq!(pending_while_starting(&rx), (false, true));
    }

    #[test]
    fn audio_target_is_bounded_after_a_clock_jump() {
        let rate = audio::SAMPLE_RATE as u64;
        assert_eq!(audio_target(rate * 10, rate * 9, false, None), rate * 10 - rate / 4);
        assert_eq!(audio_target(rate * 10, rate * 9, true, None), rate * 10);
        let hour = rate * 3600;
        assert_eq!(audio_target(hour, rate, true, None), rate + rate * AUDIO_LEAD_SECONDS);
        assert_eq!(audio_target(hour, rate, true, Some(rate * 2)), rate * 2);
        assert_eq!(frames_since(10_000_005, 5), rate);
    }

    #[test]
    fn partial_names_round_trip() {
        let path = std::path::Path::new(r"C:\captures\wuwa\wuwa 2026-09-24 12-00-00.mp4");
        let partial = partial_path(path, 1);
        let name = partial.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "wuwa 2026-09-24 12-00-00.mp4.1.partial");
        assert_eq!(partial.parent(), path.parent());
        assert!(is_partial_name(&name));
        assert!(!is_partial_name("wuwa 2026-09-24 12-00-00.mp4"));
        assert!(!is_partial_name("notes.partial"));
        assert_eq!(intended_name(&name), Some("wuwa 2026-09-24 12-00-00.mp4"));
        assert_eq!(intended_name("wuwa.mp4.x.partial"), None);
        let marker = finished_marker(&partial);
        let marker_name = marker.file_name().unwrap().to_string_lossy().to_string();
        assert!(!is_partial_name(&marker_name));
    }

    fn mp4_box(kind: &[u8; 4], body: usize) -> Vec<u8> {
        let mut out = ((8 + body) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.resize(8 + body, 0);
        out
    }

    #[test]
    fn sweep_recovers_finished_clips_and_removes_the_rest() {
        let root = std::env::temp_dir().join(format!("peebify-sweep-{}", uuid::Uuid::new_v4()));
        let dir = root.join("wuwa");
        std::fs::create_dir_all(root.join(super::super::capture::THUMB_DIR)).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        let finished: Vec<u8> = [mp4_box(b"ftyp", 16), mp4_box(b"mdat", 64), mp4_box(b"moov", 32)]
            .concat();
        let mut unfinished = mp4_box(b"ftyp", 16);
        unfinished.extend_from_slice(&[0, 0, 0, 0]);
        unfinished.extend_from_slice(b"mdat");
        unfinished.resize(unfinished.len() + 64, 0);

        let with_moov = dir.join("wuwa 2026-09-24 12-00-00.mp4.1.partial");
        let marked = dir.join("wuwa 2026-09-24 13-00-00.mp4.1.partial");
        let broken = dir.join("wuwa 2026-09-24 14-00-00.mp4.1.partial");
        let stale = dir.join("wuwa 2026-09-24 15-00-00.mp4.1.partial.finished");
        std::fs::write(&with_moov, &finished).unwrap();
        std::fs::write(&marked, &unfinished).unwrap();
        std::fs::write(finished_marker(&marked), b"").unwrap();
        std::fs::write(&broken, &unfinished).unwrap();
        std::fs::write(&stale, b"").unwrap();
        let thumb = super::super::capture::thumb_path(&root, &dir.join("wuwa 2026-09-24 12-00-00.mp4"));
        std::fs::write(&thumb, b"jpg").unwrap();

        assert!(has_moov(&with_moov));
        assert!(!has_moov(&broken));

        sweep_partials(&dir, &root);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        let thumb_kept = thumb.is_file();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            names,
            ["wuwa 2026-09-24 12-00-00.mp4", "wuwa 2026-09-24 13-00-00.mp4"]
        );
        assert!(thumb_kept);
    }
}
