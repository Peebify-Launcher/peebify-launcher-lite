// ------------ Screenshot Capture ------------
// Takes screenshots of the game window (Windows only, through Windows Graphics Capture), saves them into a per-game folder and keeps small thumbnails for the gallery.
// It also lists, opens, shows and deletes captures for the overlay; clips from the recorder show up in the same list.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use windows::core::{Interface, Ref, HRESULT, PCWSTR};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::Shell::{
    IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
};

use super::BackendState;
use super::{arg_str, err_response, ok_with};

const FRAME_POOL_DEPTH: i32 = 3;

const FRAME_TIMEOUT_MS: u64 = 1500;

pub fn is_supported() -> bool {
    windows::Graphics::Capture::GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

// ------------ Window Frame Grab ------------
// Sets up Direct3D and grabs a single frame from the game window, cropped to the visible game area. The recorder
// uses the same device and capture item setup.
pub(super) struct Device {
    pub(super) device: ID3D11Device,
    pub(super) context: ID3D11DeviceContext,
    pub(super) winrt: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice,
}

pub(super) fn create_device(flags: D3D11_CREATE_DEVICE_FLAG) -> Result<Device, String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| format!("could not create a Direct3D device: {e}"))?;

    let device = device.ok_or("Direct3D returned no device")?;
    let context = context.ok_or("Direct3D returned no context")?;

    let dxgi: IDXGIDevice = device
        .cast()
        .map_err(|e| format!("could not get the DXGI device: {e}"))?;
    let winrt = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .map_err(|e| format!("could not wrap the Direct3D device: {e}"))?
        .cast()
        .map_err(|e| format!("could not cast the Direct3D device: {e}"))?;

    Ok(Device {
        device,
        context,
        winrt,
    })
}

pub(super) fn window_item(hwnd: HWND, recording: bool) -> Result<(GraphicsCaptureItem, SizeInt32), String> {
    let (done, what) = if recording { ("recorded", "record") } else { ("captured", "capture") };
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| format!("Windows would not provide the capture factory. {e}"))?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }
        .map_err(|e| format!("That window cannot be {done}. {e}"))?;
    let size = item
        .Size()
        .map_err(|e| format!("could not read the window size: {e}"))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err(format!("The window has no size to {what}."));
    }
    Ok((item, size))
}

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Region {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) fn client_region(hwnd: HWND) -> Option<Region> {
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    let client = super::overlay::client_rect(hwnd.0)?;
    let mut bounds = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let code = unsafe {
        DwmGetWindowAttribute(
            hwnd.0,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut bounds as *mut windows_sys::Win32::Foundation::RECT).cast(),
            std::mem::size_of::<windows_sys::Win32::Foundation::RECT>() as u32,
        )
    };
    if code < 0 {
        return None;
    }
    Some(Region {
        x: u32::try_from(client.x.checked_sub(bounds.left)?).ok()?,
        y: u32::try_from(client.y.checked_sub(bounds.top)?).ok()?,
        width: u32::try_from(client.width).ok()?,
        height: u32::try_from(client.height).ok()?,
    })
}

pub(super) fn visible_region(wanted: Option<Region>, width: u32, height: u32) -> Region {
    let full = Region {
        x: 0,
        y: 0,
        width,
        height,
    };
    let Some(wanted) = wanted else {
        return full;
    };
    if wanted.x >= width || wanted.y >= height {
        return full;
    }
    let region = Region {
        x: wanted.x,
        y: wanted.y,
        width: wanted.width.min(width - wanted.x),
        height: wanted.height.min(height - wanted.y),
    };
    if region.width == 0 || region.height == 0 {
        full
    } else {
        region
    }
}

pub fn capture_window(hwnd: HWND) -> Result<Frame, String> {
    if !is_supported() {
        return Err("This version of Windows cannot capture a window.".to_string());
    }
    if super::overlay::is_exclusive_fullscreen(hwnd.0) {
        return Err(
            "The game is in exclusive fullscreen, which Windows cannot capture. Switch it to Borderless or Windowed."
                .to_string(),
        );
    }

    let device = create_device(D3D11_CREATE_DEVICE_BGRA_SUPPORT)?;
    let (item, size) = window_item(hwnd, false)?;

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device.winrt,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        FRAME_POOL_DEPTH,
        size,
    )
    .map_err(|e| format!("could not create the capture pool: {e}"))?;

    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|e| format!("could not start capturing: {e}"))?;
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetIsCursorCaptureEnabled(false);
    let region = client_region(hwnd);
    session
        .StartCapture()
        .map_err(|e| format!("could not start capturing: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(FRAME_TIMEOUT_MS);
    let frame = loop {
        if let Ok(frame) = pool.TryGetNextFrame() {
            break Some(frame);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    };

    let result = match frame {
        Some(frame) => read_frame(&device, &frame, region),
        None => {
            Err("The game did not draw a frame in time. It may be paused or minimised.".to_string())
        }
    };

    let _ = session.Close();
    let _ = pool.Close();
    result
}

fn read_frame(
    device: &Device,
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    wanted: Option<Region>,
) -> Result<Frame, String> {
    let surface = frame
        .Surface()
        .map_err(|e| format!("the frame had no surface: {e}"))?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .map_err(|e| format!("could not reach the frame's texture: {e}"))?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
        .map_err(|e| format!("could not reach the frame's texture: {e}"))?;
    let content = frame
        .ContentSize()
        .ok()
        .and_then(|size| Some((u32::try_from(size.Width).ok()?, u32::try_from(size.Height).ok()?)));
    read_texture(&device.device, &device.context, &texture, wanted, content)
}

pub(super) fn read_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    wanted: Option<Region>,
    content: Option<(u32, u32)>,
) -> Result<Frame, String> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        BindFlags: 0,
        MiscFlags: 0,
        ..desc
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
        .map_err(|e| format!("could not allocate a readback texture: {e}"))?;
    let staging = staging.ok_or("Direct3D returned no readback texture")?;

    unsafe { context.CopyResource(&staging, texture) };

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|e| format!("could not read the captured frame: {e}"))?;
    if mapped.pData.is_null() {
        unsafe { context.Unmap(&staging, 0) };
        return Err("Direct3D mapped no frame data".to_string());
    }

    let (content_width, content_height) = content
        .filter(|(w, h)| *w > 0 && *h > 0)
        .unwrap_or((desc.Width, desc.Height));
    let region = visible_region(
        wanted,
        content_width.min(desc.Width),
        content_height.min(desc.Height),
    );
    let width = region.width;
    let height = region.height;
    let row_bytes = (width * 4) as usize;
    let column_offset = (region.x * 4) as usize;
    let mut bgra = vec![0u8; row_bytes * height as usize];

    for row in 0..height as usize {
        let source = unsafe {
            std::slice::from_raw_parts(
                (mapped.pData as *const u8)
                    .add((region.y as usize + row) * mapped.RowPitch as usize + column_offset),
                row_bytes,
            )
        };
        bgra[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(source);
    }

    unsafe { context.Unmap(&staging, 0) };

    Ok(Frame {
        width,
        height,
        bgra,
    })
}

fn bgra_into_rgb(mut pixels: Vec<u8>) -> Vec<u8> {
    let count = pixels.len() / 4;
    for i in 0..count {
        let (b, g, r) = (pixels[i * 4], pixels[i * 4 + 1], pixels[i * 4 + 2]);
        pixels[i * 3] = r;
        pixels[i * 3 + 1] = g;
        pixels[i * 3 + 2] = b;
    }
    pixels.truncate(count * 3);
    pixels
}

pub(super) fn frame_to_rgb(frame: Frame) -> Result<image::RgbImage, String> {
    let Frame {
        width,
        height,
        bgra,
    } = frame;
    image::RgbImage::from_raw(width, height, bgra_into_rgb(bgra))
        .ok_or_else(|| "the captured frame was the wrong size".to_string())
}

// ------------ Names and Thumbnails ------------
// How capture files are named and where they live, plus the small preview images, including the background pass that fills in missing ones.
pub fn timestamped_name(game_id: &str, extension: &str) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    format!("{} {now}.{extension}", name_prefix(game_id))
}

fn name_prefix(game_id: &str) -> String {
    if super::game_profiles::is_known_game_id(game_id) {
        return game_id.to_string();
    }
    let slug = super::fs_util::sanitize_folder_name(game_id).replace(' ', "_");
    if slug.is_empty() {
        super::game_profiles::DEFAULT_GAME_ID.to_string()
    } else {
        slug
    }
}

pub(super) fn capture_path(directory: &Path, game_id: &str, extension: &str) -> PathBuf {
    unique_path(directory, &timestamped_name(game_id, extension))
}

pub(super) fn unique_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (file_name.to_string(), String::new()),
    };
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem} ({}){ext}", chrono::Utc::now().timestamp()))
}

fn taken_at_from_name(name: &str) -> Option<i64> {
    use chrono::TimeZone;
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let (_, rest) = stem.split_once(' ')?;
    let stamp = rest.get(..19)?;
    let naive = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H-%M-%S").ok()?;
    let local = chrono::Local.from_local_datetime(&naive);
    local
        .single()
        .or_else(|| local.earliest())
        .map(|t| t.timestamp_millis())
}

fn capture_kind(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" => "screenshot",
        "mp4" | "mkv" | "webm" => "clip",
        _ => return None,
    })
}

pub(super) const THUMB_DIR: &str = ".thumbs";

pub(super) fn thumb_path(root: &Path, capture: &Path) -> PathBuf {
    let stem = capture
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    root.join(THUMB_DIR).join(format!("pending-{stem}.jpg"))
}

fn screenshot_thumb(root: &Path, capture: &Path) -> PathBuf {
    let name = capture
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    root.join(THUMB_DIR).join(format!("shot-{name}.jpg"))
}

fn capture_thumb(root: &Path, capture: &Path) -> Option<PathBuf> {
    match capture_kind(capture)? {
        "clip" => Some(thumb_path(root, capture)),
        _ => Some(screenshot_thumb(root, capture)),
    }
}

pub(super) const THUMB_WIDTH: u32 = 320;
pub(super) const THUMB_QUALITY: u8 = 80;

fn hide_dir(dir: &Path) {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, INVALID_FILE_ATTRIBUTES,
    };
    let wide = super::process_utils::wide_path(dir);
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES || attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        return;
    }
    if unsafe { SetFileAttributesW(wide.as_ptr(), attrs | FILE_ATTRIBUTE_HIDDEN) } == 0 {
        log::debug!("capture: could not hide {}", dir.display());
    }
}

fn thumb_size(width: u32, height: u32) -> (u32, u32) {
    if width <= THUMB_WIDTH {
        return (width, height);
    }
    let scaled = (height as u64 * THUMB_WIDTH as u64 / width as u64).max(1) as u32;
    (THUMB_WIDTH, scaled)
}

pub(super) fn write_thumbnail(image: &image::RgbImage, dest: &Path) -> Result<(), String> {
    let (width, height) = image.dimensions();
    let (thumb_width, thumb_height) = thumb_size(width, height);
    let scaled;
    let small = if thumb_width < width {
        scaled = image::imageops::thumbnail(image, thumb_width, thumb_height);
        &scaled
    } else {
        image
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| super::fs_util::fmt_io("Could not create the thumbnail folder", &e))?;
        hide_dir(parent);
    }
    let tmp = PathBuf::from(format!("{}.tmp", dest.display()));
    let written = (|| {
        let file = std::fs::File::create(&tmp)
            .map_err(|e| super::fs_util::fmt_io("Could not write the thumbnail", &e))?;
        let mut writer = std::io::BufWriter::new(file);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, THUMB_QUALITY)
            .encode_image(small)
            .map_err(|e| format!("Could not encode the thumbnail: {e}"))?;
        writer
            .into_inner()
            .map_err(|e| super::fs_util::fmt_io("Could not write the thumbnail", e.error()))?;
        super::fs_util::finalize_replace(&tmp, dest)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

const BACKFILL_BATCH: usize = 200;
const BACKFILL_PAUSE: std::time::Duration = std::time::Duration::from_millis(150);

static BACKFILLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static BACKFILL_FAILED: parking_lot::Mutex<Option<std::collections::HashSet<PathBuf>>> =
    parking_lot::Mutex::new(None);

fn can_backfill(capture: &Path) -> bool {
    let decodable = capture
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg"));
    decodable
        && !BACKFILL_FAILED
            .lock()
            .as_ref()
            .is_some_and(|failed| failed.contains(capture))
}

fn backfill_one(capture: &Path, thumb: &Path) -> Result<(), String> {
    if thumb.is_file() || !capture.is_file() {
        return Ok(());
    }
    let decoded = image::ImageReader::open(capture)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let (width, height) = thumb_size(decoded.width(), decoded.height());
    let small = if width < decoded.width() {
        decoded.thumbnail_exact(width, height)
    } else {
        decoded
    };
    write_thumbnail(&small.to_rgb8(), thumb)
}

fn backfill_thumbs(missing: Vec<(PathBuf, PathBuf)>) {
    use std::sync::atomic::Ordering;
    if missing.is_empty() || BACKFILLING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("capture-thumbs".into())
        .spawn(move || {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            };
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
            let mut made = 0usize;
            for (capture, thumb) in missing.into_iter().take(BACKFILL_BATCH) {
                match backfill_one(&capture, &thumb) {
                    Ok(()) => made += 1,
                    Err(e) => {
                        log::debug!("capture: no thumbnail for {}: {e}", capture.display());
                        BACKFILL_FAILED
                            .lock()
                            .get_or_insert_with(Default::default)
                            .insert(capture);
                    }
                }
                std::thread::sleep(BACKFILL_PAUSE);
            }
            log::debug!("capture: made {made} screenshot thumbnail(s)");
            BACKFILLING.store(false, Ordering::Release);
        });
    if let Err(e) = spawned {
        log::warn!("capture: could not start making thumbnails: {e}");
        BACKFILLING.store(false, Ordering::Release);
    }
}

pub(super) fn game_folder(game_id: &str) -> String {
    if super::game_profiles::is_known_game_id(game_id) {
        let pretty = super::fs_util::sanitize_folder_name(super::game_profiles::display_name(
            super::game_profiles::profile(game_id),
        ));
        if !pretty.is_empty() {
            return pretty;
        }
    }
    let slug = super::fs_util::sanitize_folder_name(game_id);
    if slug.is_empty() {
        super::game_profiles::DEFAULT_GAME_ID.to_string()
    } else {
        slug
    }
}

pub(super) fn game_id_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let slug = name.split(' ').next()?;
    super::game_profiles::is_known_game_id(slug).then(|| slug.to_string())
}

pub(super) fn blocked_message(folder: &Path) -> String {
    format!(
        "Windows would not let Peebify save to {}. If Controlled folder access is on, allow Peebify Launcher there, or choose another capture folder in Settings.",
        folder.display()
    )
}

pub(super) fn save_error(context: &str, folder: &Path, e: &std::io::Error) -> String {
    if super::fs_util::is_access_denied(e) {
        blocked_message(folder)
    } else {
        super::fs_util::fmt_io(context, e)
    }
}

// ------------ Saving Screenshots ------------
// Writes the frame out as PNG or JPEG. The overlay hotkey calls this, and a press is skipped while the last one is still being saved.
fn encode(image: &image::RgbImage, path: &Path, format: &str, quality: u8) -> Result<(), String> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    let partial = path.with_file_name(name);
    let written = encode_to(image, &partial, format, quality)
        .and_then(|()| super::fs_util::finalize_replace(&partial, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

fn encode_to(image: &image::RgbImage, path: &Path, format: &str, quality: u8) -> Result<(), String> {
    let (width, height) = image.dimensions();
    let file = std::fs::File::create(path).map_err(|e| {
        save_error(
            "Could not save the screenshot",
            path.parent().unwrap_or(path),
            &e,
        )
    })?;
    let mut writer = std::io::BufWriter::new(file);

    match format {
        "jpeg" => {
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality);
            encoder
                .encode_image(image)
                .map_err(|e| format!("Could not save the screenshot: {e}"))?;
        }
        _ => {
            let encoder = image::codecs::png::PngEncoder::new_with_quality(
                &mut writer,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Up,
            );
            use image::ImageEncoder;
            encoder
                .write_image(image, width, height, image::ExtendedColorType::Rgb8)
                .map_err(|e| format!("Could not save the screenshot: {e}"))?;
        }
    }
    writer
        .into_inner()
        .map_err(|e| super::fs_util::fmt_io("Could not save the screenshot", e.error()))?;
    Ok(())
}

static SHOOTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) async fn take_screenshot(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    if SHOOTING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        log::debug!("capture: a screenshot is already being saved, so this press was skipped");
        return Ok(ok_with(json!({ "skipped": true })));
    }
    let outcome = take_screenshot_inner(app, args).await;
    SHOOTING.store(false, std::sync::atomic::Ordering::Release);
    let failed = match &outcome {
        Ok(value) if value.get("success") == Some(&Value::Bool(false)) => Some(
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("The screenshot failed."),
        ),
        Err(e) => Some(e.as_str()),
        _ => None,
    };
    if let Some(error) = failed {
        let _ = app.emit("overlay-capture-failed", json!({ "error": error }));
    }
    outcome
}

async fn take_screenshot_inner(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(raw_hwnd) = super::overlay::game_hwnd().map(|h| h as i64) else {
        return Ok(err_response("No game is running."));
    };

    let state = app.state::<BackendState>();
    let game_id = arg_str(args, 0)
        .filter(|id| super::game_profiles::is_known_game_id(id))
        .map(str::to_string)
        .unwrap_or_else(|| state.overlay.capture_game_id());
    let format = state
        .config
        .get("behavior.overlayShotFormat")
        .as_str()
        .unwrap_or("png")
        .to_string();
    let quality = state
        .config
        .get("behavior.overlayShotQuality")
        .as_str()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(90)
        .clamp(40, 100);

    let root = super::overlay::capture_dir(app);
    let directory = root.join(game_folder(&game_id));
    if let Err(e) = std::fs::create_dir_all(&directory) {
        return Ok(err_response(save_error(
            "Could not create the captures folder",
            &directory,
            &e,
        )));
    }

    let extension = if format == "jpeg" { "jpg" } else { "png" };
    let path: PathBuf = capture_path(&directory, &game_id, extension);

    let thumb = screenshot_thumb(&root, &path);
    let outcome = tauri::async_runtime::spawn_blocking({
        let path = path.clone();
        move || -> Result<(), String> {
            let _ = unsafe {
                windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                )
            };
            let frame = capture_window(HWND(raw_hwnd as *mut core::ffi::c_void))?;
            let image = frame_to_rgb(frame)?;
            encode(&image, &path, &format, quality)?;
            if let Err(e) = write_thumbnail(&image, &thumb) {
                log::debug!("capture: no screenshot thumbnail: {e}");
            }
            Ok(())
        }
    })
    .await
    .map_err(|e| format!("the screenshot task failed: {e}"))?;

    match outcome {
        Ok(()) => {
            log::info!("capture: saved {}", path.display());
            let payload = json!({ "path": path.to_string_lossy(), "gameId": game_id });
            let _ = app.emit("overlay-capture", payload.clone());
            Ok(ok_with(payload))
        }
        Err(e) => {
            log::warn!("capture: {e}");
            Ok(err_response(e))
        }
    }
}

// ------------ Capture Gallery ------------
// Lists everything in the captures folder newest first, finds a capture again safely when you open, show or delete it,
// and opens a game's screenshot folder.
pub(super) fn collect_captures(
    dir: &Path,
    out: &mut Vec<(std::time::SystemTime, PathBuf, u64, bool)>,
) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(kind) = capture_kind(&path) else {
            continue;
        };
        let clip = kind == "clip";
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        out.push((modified, path, meta.len(), clip));
    }
}

pub(super) fn walk_captures(root: &Path) -> Vec<(std::time::SystemTime, PathBuf, u64, bool)> {
    let mut entries: Vec<(std::time::SystemTime, PathBuf, u64, bool)> = Vec::new();
    collect_captures(root, &mut entries);
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        collect_captures(&entry.path(), &mut entries);
    }
    entries
}

fn taken_at_ms(path: &Path, modified: std::time::SystemTime) -> i64 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(taken_at_from_name)
        .unwrap_or_else(|| chrono::DateTime::<chrono::Utc>::from(modified).timestamp_millis())
}

fn newest_first(
    entries: Vec<(std::time::SystemTime, PathBuf, u64, bool)>,
    max: usize,
) -> Vec<(i64, PathBuf, u64, bool)> {
    let mut dated: Vec<(i64, PathBuf, u64, bool)> = entries
        .into_iter()
        .map(|(modified, path, size, clip)| (taken_at_ms(&path, modified), path, size, clip))
        .collect();
    dated.sort_by_key(|(taken, _, _, _)| std::cmp::Reverse(*taken));
    dated.truncate(max);
    dated
}

pub(super) async fn list_captures(app: &AppHandle) -> Result<Value, String> {
    const MAX: usize = 5000;
    let directory = super::overlay::capture_dir(app);

    let root = directory.clone();
    let app = app.clone();
    let captures = tauri::async_runtime::spawn_blocking(move || {
        let listed = newest_first(walk_captures(&root), MAX);
        if root.is_dir() {
            super::fs_util::allow_asset_dir(&app, &root);
            super::fs_util::allow_asset_dir_shallow(&app, &root.join(THUMB_DIR));
        }
        let thumbs: std::collections::HashSet<std::ffi::OsString> =
            std::fs::read_dir(root.join(THUMB_DIR))
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.file_name())
                .collect();
        let mut missing = Vec::new();
        let captures = listed
            .iter()
            .map(|(taken, path, size, clip)| {
                let wanted = capture_thumb(&root, path);
                let has_thumb = wanted
                    .as_deref()
                    .and_then(Path::file_name)
                    .is_some_and(|name| thumbs.contains(name));
                let thumb = wanted.filter(|_| has_thumb);
                if !has_thumb && !*clip && can_backfill(path) {
                    missing.push((path.clone(), screenshot_thumb(&root, path)));
                }
                json!({
                    "path": path.to_string_lossy(),
                    "name": path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "sizeBytes": size,
                    "kind": if *clip { "clip" } else { "screenshot" },
                    "takenAt": chrono::DateTime::<chrono::Utc>::from_timestamp_millis(*taken)
                        .map(|t| t.to_rfc3339()),
                    "gameId": game_id_of(path),
                    "thumbPath": thumb.map(|t| t.to_string_lossy().to_string()),
                })
            })
            .collect::<Vec<Value>>();
        backfill_thumbs(missing);
        captures
    })
    .await
    .map_err(|e| format!("the capture listing failed: {e}"))?;

    Ok(ok_with(json!({
        "captures": captures,
        "folder": directory.to_string_lossy(),
    })))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Located {
    Present(PathBuf),
    Missing(PathBuf),
}

pub(super) fn inside_capture_dir(app: &AppHandle, raw: &str) -> Option<Located> {
    let configured = super::overlay::capture_dir(app);
    if !lexically_local_to(&configured, raw) {
        return None;
    }
    let directory = configured.canonicalize().ok()?;
    locate_capture(&directory, Path::new(raw))
}

fn lexically_local_to(configured: &Path, raw: &str) -> bool {
    let norm = |text: &str| text.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let raw = norm(raw);
    if !raw.starts_with(r"\\") {
        return true;
    }
    let dir = norm(&configured.to_string_lossy());
    !dir.is_empty() && raw.starts_with(&format!("{dir}\\"))
}

fn locate_capture(root: &Path, raw: &Path) -> Option<Located> {
    use std::path::Component;
    if !raw.is_absolute()
        || capture_kind(raw).is_none()
        || raw
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    match raw.canonicalize() {
        Ok(path) => (path.starts_with(root) && capture_kind(&path).is_some())
            .then_some(Located::Present(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut tail = Vec::new();
            let mut base = raw;
            let resolved = loop {
                tail.push(base.file_name()?);
                base = base.parent()?;
                if let Ok(dir) = base.canonicalize() {
                    break dir;
                }
            };
            let path = tail.iter().rev().fold(resolved, |path, part| path.join(part));
            path.starts_with(root).then_some(Located::Missing(path))
        }
        Err(_) => None,
    }
}

const MISSING_MSG: &str = "That capture no longer exists. It may have been moved or deleted.";

fn emit_deleted(app: &AppHandle, raw: &str) {
    let _ = app.emit("overlay-capture-deleted", json!({ "path": raw }));
}

fn missing_response(app: &AppHandle, raw: &str) -> Value {
    emit_deleted(app, raw);
    let mut response = err_response(MISSING_MSG);
    response["code"] = json!("missing");
    response
}

pub(super) async fn delete_capture(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(raw) = arg_str(args, 0) else {
        return Ok(err_response("No file was given."));
    };
    let path = match inside_capture_dir(app, raw) {
        Some(Located::Present(path)) => path,
        Some(Located::Missing(path)) => {
            if let Some(thumb) = capture_thumb(&super::overlay::capture_dir(app), &path) {
                let _ = std::fs::remove_file(thumb);
            }
            log::info!("capture: {} was already gone", path.display());
            emit_deleted(app, raw);
            return Ok(super::ok_response());
        }
        None => return Ok(err_response("That file is not in the captures folder.")),
    };
    let thumb = capture_thumb(&super::overlay::capture_dir(app), &path);
    let target = path.clone();
    let stopped = || RecycleError::Io(std::io::Error::other("the delete task stopped"));
    let recycled = tauri::async_runtime::spawn_blocking(move || {
        std::thread::spawn(move || recycle_file(&target))
            .join()
            .unwrap_or_else(|_| Err(stopped()))
    })
    .await
    .unwrap_or_else(|_| Err(stopped()));
    match recycled {
        Ok(()) => {
            if let Some(thumb) = thumb {
                let _ = std::fs::remove_file(thumb);
            }
            log::info!("capture: moved {} to the Recycle Bin", path.display());
            emit_deleted(app, raw);
            Ok(super::ok_response())
        }
        Err(RecycleError::NotRecyclable) => {
            log::warn!(
                "capture: kept {}, it cannot go to the Recycle Bin",
                path.display()
            );
            let mut response = err_response(NOT_RECYCLABLE_MSG);
            response["code"] = json!("not-recyclable");
            Ok(response)
        }
        Err(RecycleError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(thumb) = thumb {
                let _ = std::fs::remove_file(thumb);
            }
            emit_deleted(app, raw);
            Ok(super::ok_response())
        }
        Err(RecycleError::Io(e)) => Ok(err_response(super::fs_util::fmt_io(
            "Could not delete that capture",
            &e,
        ))),
    }
}

pub(super) async fn open_screenshot_folder(
    app: &AppHandle,
    args: &[Value],
) -> Result<Value, String> {
    let game_id = arg_str(args, 0)
        .filter(|id| super::game_profiles::is_known_game_id(id))
        .map(str::to_string)
        .unwrap_or_else(|| super::active_game_id(app));
    let root = super::overlay::capture_dir(app);
    let per_game = root.join(game_folder(&game_id));
    let dir = if per_game.is_dir() { per_game } else { root };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Ok(err_response(format!("Could not open that folder: {e}")));
    }
    super::fs_util::dialog::open_folder(&dir)?;
    Ok(super::ok_response())
}

// ------------ Recycle Bin Delete ------------
// Deleting a capture sends it to the Recycle Bin through the Windows shell, and refuses to delete it for good if the bin cannot take it.
const NOT_RECYCLABLE_MSG: &str = "This capture can't go to the Recycle Bin (it may be too large for it, or this drive has none), so it was not deleted.";

enum RecycleError {
    NotRecyclable,
    Io(std::io::Error),
}

#[windows_core::implement(IFileOperationProgressSink)]
struct RecycleOnly {
    refused: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

macro_rules! accept_all {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(fn $name(&self, $($arg: $ty),*) -> windows::core::Result<()> {
            Ok(())
        })*
    };
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for RecycleOnly_Impl {
    accept_all! {
        StartOperations();
        FinishOperations(_hr: HRESULT);
        PreRenameItem(_flags: u32, _item: Ref<'_, IShellItem>, _new_name: &PCWSTR);
        PostRenameItem(_flags: u32, _item: Ref<'_, IShellItem>, _new_name: &PCWSTR, _hr: HRESULT, _created: Ref<'_, IShellItem>);
        PreMoveItem(_flags: u32, _item: Ref<'_, IShellItem>, _folder: Ref<'_, IShellItem>, _new_name: &PCWSTR);
        PostMoveItem(_flags: u32, _item: Ref<'_, IShellItem>, _folder: Ref<'_, IShellItem>, _new_name: &PCWSTR, _hr: HRESULT, _created: Ref<'_, IShellItem>);
        PreCopyItem(_flags: u32, _item: Ref<'_, IShellItem>, _folder: Ref<'_, IShellItem>, _new_name: &PCWSTR);
        PostCopyItem(_flags: u32, _item: Ref<'_, IShellItem>, _folder: Ref<'_, IShellItem>, _new_name: &PCWSTR, _hr: HRESULT, _created: Ref<'_, IShellItem>);
        PostDeleteItem(_flags: u32, _item: Ref<'_, IShellItem>, _hr: HRESULT, _created: Ref<'_, IShellItem>);
        PreNewItem(_flags: u32, _folder: Ref<'_, IShellItem>, _new_name: &PCWSTR);
        PostNewItem(_flags: u32, _folder: Ref<'_, IShellItem>, _new_name: &PCWSTR, _template: &PCWSTR, _attributes: u32, _hr: HRESULT, _created: Ref<'_, IShellItem>);
        UpdateProgress(_total: u32, _done: u32);
        ResetTimer();
        PauseTimer();
        ResumeTimer();
    }
    fn PreDeleteItem(&self, flags: u32, _item: Ref<'_, IShellItem>) -> windows::core::Result<()> {
        use windows::Win32::UI::Shell::TSF_DELETE_RECYCLE_IF_POSSIBLE;
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            self.refused.store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(windows::Win32::Foundation::E_ABORT.into());
        }
        Ok(())
    }
}

fn shell_io_error(e: windows::core::Error) -> std::io::Error {
    let code = e.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        return std::io::Error::from_raw_os_error((code & 0xFFFF) as i32);
    }
    std::io::Error::other(e)
}

fn recycle_file(path: &Path) -> Result<(), RecycleError> {
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            return Err(RecycleError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "That path is not a file.",
            )))
        }
        Err(e) => return Err(RecycleError::Io(e)),
    }
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    if let Err(e) = init.ok() {
        return Err(RecycleError::Io(shell_io_error(e)));
    }
    let result = recycle_with_shell(path);
    unsafe { CoUninitialize() };
    result
}

fn recycle_with_shell(path: &Path) -> Result<(), RecycleError> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::UI::Shell::{
        FileOperation, IFileOperation, SHCreateItemFromParsingName, FOFX_EARLYFAILURE,
        FOFX_RECYCLEONDELETE, FOF_ALLOWUNDO, FOF_NO_UI,
    };
    let io = |e: windows::core::Error| RecycleError::Io(shell_io_error(e));
    let wide = peebify_helpers::common::wide(&super::fs_util::plain_path(path));
    let refused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    unsafe {
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(io)?;
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(io)?;
        op.SetOperationFlags(FOF_ALLOWUNDO | FOF_NO_UI | FOFX_RECYCLEONDELETE | FOFX_EARLYFAILURE)
            .map_err(io)?;
        let sink: IFileOperationProgressSink = RecycleOnly {
            refused: refused.clone(),
        }
        .into();
        op.DeleteItem(&item, &sink).map_err(io)?;
        let performed = op.PerformOperations();
        if refused.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(RecycleError::NotRecyclable);
        }
        performed.map_err(io)?;
        if op
            .GetAnyOperationsAborted()
            .is_ok_and(|aborted| aborted.as_bool())
        {
            return Err(RecycleError::Io(std::io::Error::other(
                "The Recycle Bin move was cancelled",
            )));
        }
    }
    Ok(())
}

pub(super) async fn reveal_capture(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(raw) = arg_str(args, 0) else {
        return Ok(err_response("No file was given."));
    };
    let path = match inside_capture_dir(app, raw) {
        Some(Located::Present(path)) => path,
        Some(Located::Missing(_)) => return Ok(missing_response(app, raw)),
        None => return Ok(err_response("That file is not in the captures folder.")),
    };
    use std::os::windows::process::CommandExt;
    let argument = format!("/select,\"{}\"", super::fs_util::plain_path(&path));
    match std::process::Command::new("explorer.exe")
        .raw_arg(argument)
        .spawn()
    {
        Ok(_) => Ok(super::ok_response()),
        Err(e) => Ok(err_response(super::fs_util::fmt_io(
            "Could not show that file",
            &e,
        ))),
    }
}

pub(super) async fn open_capture(app: &AppHandle, args: &[Value]) -> Result<Value, String> {
    let Some(raw) = arg_str(args, 0) else {
        return Ok(err_response("No file was given."));
    };
    let path = match inside_capture_dir(app, raw) {
        Some(Located::Present(path)) => path,
        Some(Located::Missing(_)) => return Ok(missing_response(app, raw)),
        None => return Ok(err_response("That file is not in the captures folder.")),
    };
    match open::that_detached(super::fs_util::plain_path(&path)) {
        Ok(_) => Ok(super::ok_response()),
        Err(e) => Ok(err_response(super::fs_util::fmt_io(
            "Could not open that file",
            &e,
        ))),
    }
}

pub(super) async fn capture_status(app: &AppHandle) -> Result<Value, String> {
    Ok(ok_with(json!({
        "supported": is_supported(),
        "recordingSupported": super::recorder::recording_supported(),
        "folder": super::overlay::capture_dir(app).to_string_lossy(),
    })))
}

// ------------ Tests ------------
// Covers capture names, thumbnails, sorting and what counts as a missing capture.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_paths_are_refused_unless_the_capture_folder_is_on_that_share() {
        let local = Path::new(r"C:\Users\me\Pictures\Peebify");
        assert!(lexically_local_to(local, r"C:\Users\me\Pictures\Peebify\a.png"));
        assert!(!lexically_local_to(local, r"\\attacker\share\Peebify\a.png"));
        assert!(!lexically_local_to(local, r"//attacker/share/a.png"));
        let share = Path::new(r"\\nas\captures");
        assert!(lexically_local_to(share, r"\\NAS\captures\wuwa\a.png"));
        assert!(!lexically_local_to(share, r"\\nas\capturesX\a.png"));
        assert!(!lexically_local_to(share, r"\\other\captures\a.png"));
    }

    fn region(x: u32, y: u32, width: u32, height: u32) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn visible_region_crops_to_the_client_area() {
        assert_eq!(
            visible_region(Some(region(8, 31, 1280, 720)), 1296, 759),
            region(8, 31, 1280, 720)
        );
    }

    #[test]
    fn visible_region_clamps_to_the_frame() {
        assert_eq!(
            visible_region(Some(region(8, 31, 1920, 1080)), 1296, 759),
            region(8, 31, 1288, 728)
        );
    }

    #[test]
    fn visible_region_falls_back_to_the_whole_frame() {
        assert_eq!(visible_region(None, 1920, 1080), region(0, 0, 1920, 1080));
        assert_eq!(
            visible_region(Some(region(2000, 0, 100, 100)), 1920, 1080),
            region(0, 0, 1920, 1080)
        );
        assert_eq!(
            visible_region(Some(region(0, 0, 0, 1080)), 1920, 1080),
            region(0, 0, 1920, 1080)
        );
    }

    #[test]
    fn name_prefix_never_leaves_the_folder() {
        assert_eq!(name_prefix("wuwa"), "wuwa");
        assert_eq!(name_prefix(r"..\..\x"), ".._.._x");
        assert_eq!(name_prefix(r"C:\Windows\evil"), "C__Windows_evil");
        assert_eq!(name_prefix(""), super::super::game_profiles::DEFAULT_GAME_ID);
        assert!(!name_prefix("my game").contains(' '));
    }

    #[test]
    fn capture_paths_never_overwrite_and_still_parse() {
        let dir = std::env::temp_dir().join(format!("peebify-capture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = capture_path(&dir, "wuwa", "png");
        std::fs::write(&first, b"x").unwrap();
        let second = capture_path(&dir, "wuwa", "png");
        std::fs::write(&second, b"x").unwrap();
        let third = capture_path(&dir, "wuwa", "png");
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_ne!(first, third);
        for path in [&first, &second, &third] {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(taken_at_from_name(name).is_some(), "{}", path.display());
            assert_eq!(game_id_of(path).as_deref(), Some("wuwa"));
        }
    }

    #[test]
    fn taken_at_parses_the_capture_filename_as_local_time() {
        use chrono::TimeZone;
        let expected = chrono::Local
            .with_ymd_and_hms(2026, 9, 17, 14, 3, 22)
            .single()
            .unwrap()
            .timestamp_millis();
        assert_eq!(taken_at_from_name("wuwa 2026-09-17 14-03-22.png"), Some(expected));
        assert_eq!(taken_at_from_name("wuwa 2026-09-17 14-03-22.mp4"), Some(expected));
        assert_eq!(taken_at_from_name("wuwa 2026-09-17 14-03-22 (2).png"), Some(expected));
        assert_eq!(taken_at_from_name("holiday.png"), None);
        assert_eq!(taken_at_from_name("wuwa notadate.png"), None);
    }

    #[test]
    fn clip_thumbnails_sit_in_the_hidden_folder_by_stem() {
        let root = Path::new(r"C:\caps");
        assert_eq!(
            thumb_path(root, Path::new(r"C:\caps\Wuthering Waves\wuwa 2026-01-01 10-00-00.mp4")),
            root.join(THUMB_DIR).join("pending-wuwa 2026-01-01 10-00-00.jpg")
        );
    }

    #[test]
    fn screenshots_never_share_a_clip_thumbnail() {
        let root = Path::new(r"C:\caps");
        let clip = Path::new(r"C:\caps\Wuthering Waves\wuwa 2026-01-01 10-00-00.mp4");
        let shot = Path::new(r"C:\caps\Wuthering Waves\wuwa 2026-01-01 10-00-00.png");
        let jpeg = Path::new(r"C:\caps\Wuthering Waves\wuwa 2026-01-01 10-00-00.jpg");
        assert_eq!(capture_thumb(root, clip), Some(thumb_path(root, clip)));
        let shot_thumb = capture_thumb(root, shot).unwrap();
        assert_eq!(
            shot_thumb,
            root.join(THUMB_DIR).join("shot-wuwa 2026-01-01 10-00-00.png.jpg")
        );
        assert_ne!(Some(shot_thumb.clone()), capture_thumb(root, clip));
        assert_ne!(Some(shot_thumb), capture_thumb(root, jpeg));
        assert_eq!(capture_thumb(root, Path::new(r"C:\caps\notes.txt")), None);
    }

    #[test]
    fn a_frame_with_too_few_pixels_is_refused() {
        let short = Frame {
            width: 2,
            height: 2,
            bgra: vec![0; 8],
        };
        assert!(frame_to_rgb(short).is_err());
    }

    #[test]
    fn thumbnails_shrink_to_the_tile_width_and_keep_the_shape() {
        assert_eq!(thumb_size(3840, 2160), (THUMB_WIDTH, 180));
        assert_eq!(thumb_size(200, 100), (200, 100));
        assert_eq!(thumb_size(100_000, 1), (THUMB_WIDTH, 1));

        let dir = std::env::temp_dir().join(format!("peebify-thumb-{}", uuid::Uuid::new_v4()));
        let dest = dir.join(THUMB_DIR).join("shot-a.png.jpg");
        let image = image::RgbImage::from_pixel(640, 360, image::Rgb([200, 10, 10]));
        write_thumbnail(&image, &dest).unwrap();
        let written = image::open(&dest).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((written.width(), written.height()), (THUMB_WIDTH, 180));
    }

    #[test]
    fn captures_removed_outside_peebify_are_missing_not_outside() {
        let base = std::env::temp_dir().join(format!("peebify-locate-{}", uuid::Uuid::new_v4()));
        let root = base.join("caps");
        std::fs::create_dir_all(root.join("Game")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        let present = root.join("Game").join("a.png");
        std::fs::write(&present, b"x").unwrap();
        let canonical_root = root.canonicalize().unwrap();
        let canonical_present = present.canonicalize().unwrap();

        let here = locate_capture(&canonical_root, &present);
        let gone = locate_capture(&canonical_root, &root.join("Game").join("b.png"));
        let gone_folder = locate_capture(&canonical_root, &root.join("Old").join("c.mp4"));
        let outside = locate_capture(&canonical_root, &base.join("elsewhere").join("d.png"));
        let escaping = locate_capture(
            &canonical_root,
            &root.join("Game").join("..").join("..").join("elsewhere").join("e.png"),
        );
        let not_capture = locate_capture(&canonical_root, &root.join("Game").join("notes.txt"));
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(here, Some(Located::Present(canonical_present)));
        assert!(matches!(gone, Some(Located::Missing(p)) if p.ends_with(r"Game\b.png")));
        assert!(matches!(gone_folder, Some(Located::Missing(p)) if p.ends_with(r"Old\c.mp4")));
        assert_eq!(outside, None);
        assert_eq!(escaping, None);
        assert_eq!(not_capture, None);
    }

    #[test]
    fn access_denied_saves_explain_the_blocked_folder() {
        let folder = Path::new(r"C:\Users\me\Videos\Peebify\Wuthering Waves");
        let denied = std::io::Error::from_raw_os_error(5);
        let message = save_error("Could not create the captures folder", folder, &denied);
        assert!(message.contains(r"C:\Users\me\Videos\Peebify\Wuthering Waves"));
        assert!(message.contains("Controlled folder access"));

        let full = std::io::Error::from_raw_os_error(112);
        let other = save_error("Could not create the captures folder", folder, &full);
        assert!(other.starts_with("Could not create the captures folder"));
        assert!(!other.contains("Controlled folder access"));
    }

    #[test]
    fn listing_sorts_by_the_name_stamp_before_the_modified_time() {
        let restored = std::time::SystemTime::now();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let named = PathBuf::from(r"C:\caps\wuwa 2026-01-01 10-00-00.png");
        let newer_name = PathBuf::from(r"C:\caps\wuwa 2026-02-01 10-00-00.png");
        let unnamed = PathBuf::from(r"C:\caps\holiday.png");
        let listed = newest_first(
            vec![
                (restored, named.clone(), 1, false),
                (old, unnamed.clone(), 1, false),
                (old, newer_name.clone(), 1, false),
            ],
            2,
        );
        let order: Vec<&PathBuf> = listed.iter().map(|(_, path, _, _)| path).collect();
        assert_eq!(order, vec![&newer_name, &named]);
        assert_eq!(
            listed[1].0,
            taken_at_from_name("wuwa 2026-01-01 10-00-00.png").unwrap()
        );
        assert_eq!(taken_at_ms(&unnamed, old), 1_000_000);
    }

    #[test]
    fn walking_uses_the_capture_extension_table() {
        let dir = std::env::temp_dir().join(format!("peebify-walk-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("Game")).unwrap();
        for name in ["a.webp", "b.webm", "c.png", "d.txt"] {
            std::fs::write(dir.join("Game").join(name), b"x").unwrap();
        }
        let mut found: Vec<(String, bool)> = walk_captures(&dir)
            .into_iter()
            .map(|(_, path, _, clip)| {
                (path.file_name().unwrap().to_string_lossy().to_string(), clip)
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        found.sort();
        assert_eq!(
            found,
            vec![
                ("a.webp".to_string(), false),
                ("b.webm".to_string(), true),
                ("c.png".to_string(), false),
            ]
        );
    }
}
