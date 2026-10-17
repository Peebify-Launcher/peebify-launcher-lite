// ------------ Setup Window Kit ------------
// The shared look of the setup windows, drawn with egui: Peebify colors and fonts, wallpaper backdrop, titlebar,
// step rail, buttons, cards and the progress bar, plus the v2 motion. The install wizard and uninstall window use it.

pub mod uninstall;
pub mod wizard;

use eframe::egui::{self, Color32, CornerRadius, RichText};

use motion::{Curve, Spec};

pub const LOGO_PNG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../webui/public/icons/app.png"
));

const WALLPAPER_JPG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wall.jpg"));
const WALLPAPER_BLUR_JPG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wall-blur.jpg"));

pub mod text {
    pub const MICRO: f32 = 10.5;
    pub const XS: f32 = 11.0;
    pub const SM: f32 = 11.5;
    pub const MD: f32 = 12.5;
    pub const BASE: f32 = 13.0;
    pub const BODY: f32 = 13.5;
}

pub const WINDOW: [f32; 2] = [820.0, 540.0];

const RAIL_W: f32 = 312.0;
const RAIL_PAD_X: f32 = 26.0;
const RAIL_PAD_TOP: f32 = 30.0;
const RAIL_PAD_BOTTOM: f32 = 28.0;
const PANEL_PAD_X: f32 = 38.0;
const PANEL_PAD_TOP: f32 = 34.0;
const PANEL_PAD_BOTTOM: f32 = 30.0;

pub const BG: Color32 = Color32::from_rgb(0x0b, 0x0c, 0x14);
pub const TEXT: Color32 = Color32::from_rgb(0xe9, 0xe9, 0xed);
pub const NOTICE: Color32 = Color32::from_rgb(0xe8, 0xb4, 0x4c);
pub const DANGER: Color32 = Color32::from_rgb(0xe0, 0x65, 0x5f);
pub const DANGER_TEXT: Color32 = Color32::from_rgb(0xff, 0x9b, 0x95);
const PANEL_TINT: Color32 = Color32::from_rgb(0x0e, 0x0f, 0x18);
const INK: Color32 = Color32::from_rgb(0x12, 0x13, 0x1e);

const SEMIBOLD: &str = "peebify-semibold";

pub fn tx(alpha: f32) -> Color32 {
    TEXT.gamma_multiply(alpha)
}

// ------------ Motion ------------
// Easing curves and tweens for state changes (stepper, badge, mood). Screens and text appear instantly. With
// Windows animations turned off, every tween becomes a short fade with no movement.
pub mod motion {
    use eframe::egui;

    #[derive(Clone, Copy, PartialEq, Debug)]
    pub enum Curve {
        Linear,
        Ease,
        Out,
        Spring,
        InOut,
        Sine,
    }

    fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
        let sample = |a1: f32, a2: f32, t: f32| {
            let u = 1.0 - t;
            3.0 * u * u * t * a1 + 3.0 * u * t * t * a2 + t * t * t
        };
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        let mut t = x;
        for _ in 0..24 {
            let guess = sample(x1, x2, t);
            if (guess - x).abs() < 1e-5 {
                break;
            }
            if guess < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) / 2.0;
        }
        sample(y1, y2, t)
    }

    impl Curve {
        pub fn at(self, t: f32) -> f32 {
            let t = t.clamp(0.0, 1.0);
            if t <= 0.0 || t >= 1.0 {
                return t;
            }
            match self {
                Curve::Linear => t,
                Curve::Ease => bezier(0.25, 0.1, 0.25, 1.0, t),
                Curve::Out => bezier(0.22, 1.0, 0.36, 1.0, t),
                Curve::Spring => bezier(0.34, 1.56, 0.64, 1.0, t),
                Curve::InOut => bezier(0.65, 0.0, 0.35, 1.0, t),
                Curve::Sine => bezier(0.42, 0.0, 0.58, 1.0, t),
            }
        }
    }

    pub fn reduced() -> bool {
        static REDUCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *REDUCED.get_or_init(|| !crate::win::animations_enabled())
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    pub struct Spec {
        pub secs: f32,
        pub delay: f32,
        pub curve: Curve,
    }

    impl Spec {
        pub const fn new(ms: u32, curve: Curve) -> Self {
            Self {
                secs: ms as f32 / 1000.0,
                delay: 0.0,
                curve,
            }
        }

        pub const fn after(self, ms: u32) -> Self {
            Self {
                delay: ms as f32 / 1000.0,
                ..self
            }
        }

        fn effective(self) -> Self {
            if reduced() && self.secs > 0.0 {
                Self::new(160, Curve::Linear)
            } else {
                self
            }
        }

        pub fn at(self, elapsed: f32) -> f32 {
            let spec = self.effective();
            if elapsed < spec.delay {
                return 0.0;
            }
            if spec.secs <= 0.0 {
                return 1.0;
            }
            spec.curve.at((elapsed - spec.delay) / spec.secs)
        }

        pub fn total(self) -> f32 {
            let spec = self.effective();
            spec.delay + spec.secs
        }
    }

    pub fn now(ctx: &egui::Context) -> f64 {
        ctx.input(|i| i.time)
    }

    #[derive(Clone, Copy)]
    struct Tween {
        from: f32,
        to: f32,
        start: f64,
        spec: Spec,
    }

    impl Tween {
        fn value(&self, now: f64) -> f32 {
            let k = self.spec.at((now - self.start) as f32);
            self.from + (self.to - self.from) * k
        }
    }

    pub fn tween(ctx: &egui::Context, id: egui::Id, target: f32, spec: Spec) -> f32 {
        let now = now(ctx);
        let tween = ctx.data_mut(|d| {
            let tween = match d.get_temp::<Tween>(id) {
                Some(tween) if tween.to == target => tween,
                Some(tween) => Tween {
                    from: tween.value(now),
                    to: target,
                    start: now,
                    spec,
                },
                None => Tween {
                    from: target,
                    to: target,
                    start: now,
                    spec,
                },
            };
            d.insert_temp(id, tween);
            tween
        });
        if ((now - tween.start) as f32) < tween.spec.total() {
            ctx.request_repaint();
        }
        tween.value(now)
    }

    pub fn flag(on: bool) -> f32 {
        if on {
            1.0
        } else {
            0.0
        }
    }

    pub fn drift(ctx: &egui::Context) -> f32 {
        if reduced() {
            return 0.0;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
        let t = (now(ctx) % 80.0) as f32;
        let swing = if t < 40.0 { t / 40.0 } else { 2.0 - t / 40.0 };
        Curve::Sine.at(swing)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mood {
    pub brightness: f32,
    pub saturation: f32,
    pub dim: f32,
}

impl Mood {
    const fn new(brightness: f32, saturation: f32, dim: f32) -> Self {
        Self {
            brightness,
            saturation,
            dim,
        }
    }

    pub const CALM: Mood = Mood::new(1.0, 1.0, 0.0);
    pub const INSTALLING: Mood = Mood::new(0.88, 0.95, 0.08);
    pub const DONE: Mood = Mood::new(1.06, 1.1, 0.0);
    pub const FAILED: Mood = Mood::new(0.8, 0.35, 0.12);
    pub const UNINSTALL: Mood = Mood::new(0.9, 0.6, 0.06);
    pub const REMOVING: Mood = Mood::new(0.85, 0.5, 0.12);
    pub const REMOVED: Mood = Mood::new(0.85, 0.4, 0.14);

    fn eased(self, ctx: &egui::Context) -> Self {
        let filter = Spec::new(1000, Curve::Out);
        let id = egui::Id::new("peebify-mood");
        Self {
            brightness: motion::tween(ctx, id.with("b"), self.brightness, filter),
            saturation: motion::tween(ctx, id.with("s"), self.saturation, filter),
            dim: motion::tween(ctx, id.with("d"), self.dim, Spec::new(900, Curve::Out)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub enum Step {
    Setup,
    Install,
    Ready,
}

#[derive(Clone, Copy, PartialEq, Default)]
pub enum Badge {
    #[default]
    None,
    Done,
    Failed,
}

#[derive(Default)]
pub struct Rail<'a> {
    pub step: Option<Step>,
    pub progress: f32,
    pub error: bool,
    pub muted: bool,
    pub badge: Badge,
    pub logo: Option<&'a egui::TextureHandle>,
    pub identity: String,
}

// ------------ Window And Theme ------------
// Creating the borderless window, its icon, fonts and the dark theme.
pub fn viewport(size: [f32; 2]) -> egui::ViewportBuilder {
    let mut builder = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_decorations(false)
        .with_transparent(false)
        .with_resizable(false);
    if let Some(icon) = load_icon() {
        builder = builder.with_icon(std::sync::Arc::new(icon));
    }
    builder
}

pub fn clear_color() -> [f32; 4] {
    let [r, g, b, _] = BG.to_normalized_gamma_f32();
    [r, g, b, 1.0]
}

#[derive(Default)]
pub struct Chrome {
    applied: bool,
}

impl Chrome {
    pub fn apply(&mut self) {
        if !self.applied {
            self.applied = crate::win::style_own_windows();
        }
    }
}

fn logo_rgba() -> &'static (Vec<u8>, u32, u32) {
    static LOGO: std::sync::OnceLock<(Vec<u8>, u32, u32)> = std::sync::OnceLock::new();
    LOGO.get_or_init(|| match image::load_from_memory(LOGO_PNG) {
        Ok(img) => {
            let img = img.into_rgba8();
            let (w, h) = img.dimensions();
            (img.into_raw(), w, h)
        }
        Err(_) => (Vec::new(), 0, 0),
    })
}

fn load_icon() -> Option<egui::IconData> {
    let (rgba, width, height) = logo_rgba();
    if rgba.is_empty() {
        return None;
    }
    Some(egui::IconData {
        rgba: rgba.clone(),
        width: *width,
        height: *height,
    })
}

// ------------ Warm Start ------------
// Reading the two fonts and decoding the wallpapers takes a noticeable moment, so a thread starts on it before the
// window is created and the first frame takes the results. Anything it did not get is loaded the usual way.
const FONT_FILES: [&str; 2] = ["segoeui.ttf", "seguisb.ttf"];
const WALLS: [(&str, &[u8]); 2] = [("wall", WALLPAPER_JPG), ("wall-blur", WALLPAPER_BLUR_JPG)];

#[derive(Default)]
struct Warm {
    fonts: Vec<(&'static str, Vec<u8>)>,
    walls: Vec<(&'static str, egui::ColorImage, egui::ColorImage)>,
}

static WARMING: std::sync::Mutex<Option<std::thread::JoinHandle<Warm>>> = std::sync::Mutex::new(None);
static WARMED: std::sync::Mutex<Option<Warm>> = std::sync::Mutex::new(None);

fn prewarm() {
    let handle = std::thread::Builder::new().name("setup-warm".into()).spawn(|| Warm {
        fonts: FONT_FILES
            .iter()
            .filter_map(|file| Some((*file, std::fs::read(font_dir().join(file)).ok()?)))
            .collect(),
        walls: WALLS
            .iter()
            .filter_map(|(key, bytes)| {
                let (color, gray) = decode_wall(bytes)?;
                Some((*key, color, gray))
            })
            .collect(),
    });
    if let (Ok(handle), Ok(mut slot)) = (handle, WARMING.lock()) {
        *slot = Some(handle);
    }
}

fn warmed<T>(take: impl FnOnce(&mut Warm) -> Option<T>) -> Option<T> {
    let handle = WARMING.lock().ok()?.take();
    let mut warmed = WARMED.lock().ok()?;
    if let Some(warm) = handle.and_then(|h| h.join().ok()) {
        *warmed = Some(warm);
    }
    warmed.as_mut().and_then(take)
}

fn font_dir() -> std::path::PathBuf {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("Fonts")
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let font_dir = font_dir();
    let mut load = |name: &str, file: &str| -> bool {
        let warm = warmed(|w| {
            let at = w.fonts.iter().position(|(f, _)| *f == file)?;
            Some(w.fonts.swap_remove(at).1)
        });
        match warm.or_else(|| std::fs::read(font_dir.join(file)).ok()) {
            Some(bytes) => {
                fonts
                    .font_data
                    .insert(name.to_owned(), egui::FontData::from_owned(bytes).into());
                true
            }
            None => false,
        }
    };

    let regular = load("peebify-ui", FONT_FILES[0]);
    let semibold = load(SEMIBOLD, FONT_FILES[1]);

    if regular {
        if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            list.insert(0, "peebify-ui".to_owned());
        }
    }
    let mut chain = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if semibold {
        chain.insert(0, SEMIBOLD.to_owned());
    }
    fonts
        .families
        .insert(egui::FontFamily::Name(SEMIBOLD.into()), chain);

    ctx.set_fonts(fonts);
}

pub fn semibold(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text)
        .size(size)
        .family(egui::FontFamily::Name(SEMIBOLD.into()))
        .color(Color32::WHITE)
}

pub fn semibold_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(SEMIBOLD.into()))
}

pub fn page_title(ui: &mut egui::Ui, gap: f32, text: &str, size: f32) {
    ui.scope(|g| {
        g.add_space(gap);
        g.label(semibold(text, size));
    });
}

pub fn caption(ui: &mut egui::Ui, body: impl Into<String>, color: Color32) {
    ui.label(RichText::new(body).size(text::XS).color(color));
}

pub fn apply_theme(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut style = (*ctx.global_style()).clone();
    let mut visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = Color32::TRANSPARENT;
    visuals.window_fill = BG;
    visuals.selection.bg_fill = Color32::from_white_alpha(52);
    visuals.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(4);
        w.bg_fill = Color32::TRANSPARENT;
        w.weak_bg_fill = Color32::TRANSPARENT;
        w.bg_stroke = egui::Stroke::new(1.0, tx(0.16));
    }
    visuals.widgets.noninteractive.weak_bg_fill = PANEL_TINT;
    visuals.extreme_bg_color = tx(0.05);
    style.visuals = visuals;
    style.interaction.selectable_labels = false;
    style.spacing.button_padding = egui::vec2(16.0, 8.0);
    style.spacing.item_spacing.y = 6.0;
    ctx.set_global_style(style);
}

// ------------ Backdrop And Titlebar ------------
// Loads the logo and wallpaper and paints the backdrop: the slow drift, the per screen mood (brightness, saturation,
// dim) and the frosted pane, then the drag strip and the minimize and close buttons.
pub fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let (rgba, width, height) = logo_rgba();
    if rgba.is_empty() {
        return None;
    }
    let size = [*width as usize, *height as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba);
    Some(ctx.load_texture("peebify-logo", color, egui::TextureOptions::LINEAR))
}

fn grayscale(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .flat_map(|px| {
            let luma = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
            let l = luma.round().clamp(0.0, 255.0) as u8;
            [l, l, l, px[3]]
        })
        .collect()
}

fn gray_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let id = egui::Id::new("peebify-logo-gray");
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(tex);
    }
    let (rgba, width, height) = logo_rgba();
    if rgba.is_empty() {
        return None;
    }
    let size = [*width as usize, *height as usize];
    let gray = egui::ColorImage::from_rgba_unmultiplied(size, &grayscale(rgba));
    let tex = ctx.load_texture("peebify-logo-gray", gray, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    Some(tex)
}

#[derive(Clone)]
pub struct Wall {
    pub color: egui::TextureHandle,
    gray: egui::TextureHandle,
}

fn decode_wall(bytes: &[u8]) -> Option<(egui::ColorImage, egui::ColorImage)> {
    let rgba = image::load_from_memory(bytes).ok()?.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let raw = rgba.into_raw();
    let gray = egui::ColorImage::from_rgba_unmultiplied(size, &grayscale(&raw));
    let color = egui::ColorImage::from_rgba_unmultiplied(size, &raw);
    Some((color, gray))
}

fn wall_textures(ctx: &egui::Context, key: &'static str, bytes: &[u8]) -> Option<Wall> {
    let id = egui::Id::new(key);
    if let Some(wall) = ctx.data(|d| d.get_temp::<Wall>(id)) {
        return Some(wall);
    }
    let warm = warmed(|w| {
        let at = w.walls.iter().position(|(k, _, _)| *k == key)?;
        let (_, color, gray) = w.walls.swap_remove(at);
        Some((color, gray))
    });
    let (color, gray) = warm.or_else(|| decode_wall(bytes))?;
    let wall = Wall {
        color: ctx.load_texture(key, color, egui::TextureOptions::LINEAR),
        gray: ctx.load_texture(format!("{key}-gray"), gray, egui::TextureOptions::LINEAR),
    };
    ctx.data_mut(|d| d.insert_temp(id, wall.clone()));
    Some(wall)
}

pub fn wallpaper(ctx: &egui::Context, blurred: bool) -> Option<Wall> {
    if blurred {
        wall_textures(ctx, "wall-blur", WALLPAPER_BLUR_JPG)
    } else {
        wall_textures(ctx, "wall", WALLPAPER_JPG)
    }
}

pub fn cover_uv(tex_size: egui::Vec2, rect: egui::Rect, focus: (f32, f32)) -> egui::Rect {
    let scale = (rect.width() / tex_size.x).max(rect.height() / tex_size.y);
    let visible = egui::vec2(rect.width() / scale, rect.height() / scale);
    let free = tex_size - visible;
    let u0 = (free.x * focus.0) / tex_size.x;
    let v0 = (free.y * focus.1) / tex_size.y;
    egui::Rect::from_min_max(
        egui::pos2(u0, v0),
        egui::pos2(u0 + visible.x / tex_size.x, v0 + visible.y / tex_size.y),
    )
}

pub fn gradient(painter: &egui::Painter, rect: egui::Rect, vertical: bool, stops: &[(f32, Color32)]) {
    let mut mesh = egui::Mesh::default();
    for (i, (t, color)) in stops.iter().enumerate() {
        let (a, b) = if vertical {
            let y = rect.min.y + rect.height() * t;
            (egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y))
        } else {
            let x = rect.min.x + rect.width() * t;
            (egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y))
        };
        let at = mesh.vertices.len() as u32;
        mesh.colored_vertex(a, *color);
        mesh.colored_vertex(b, *color);
        if i > 0 {
            mesh.add_triangle(at - 2, at - 1, at);
            mesh.add_triangle(at - 1, at, at + 1);
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

fn with_alpha(color: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), (alpha * 255.0).round() as u8)
}

fn scrim_color(alpha: f32) -> Color32 {
    with_alpha(BG, alpha)
}

const SCRIM_TOP: f32 = 0.50;
const SCRIM_BOTTOM: f32 = 0.72;
const WALL_FOCUS: (f32, f32) = (0.32, 0.5);

fn wall_rect(window: egui::Rect, scale: f32, drift: f32) -> egui::Rect {
    let grow = scale * (1.0 + 0.06 * drift);
    let shift = egui::vec2(-0.014 * window.width(), -0.008 * window.height()) * drift * grow;
    egui::Rect::from_center_size(window.center() + shift, window.size() * grow)
}

fn paint_wall(painter: &egui::Painter, wall: &Wall, dest: egui::Rect, uv: egui::Rect, mood: Mood) {
    let level = (mood.brightness.min(1.0) * 255.0).round() as u8;
    let tint = Color32::from_gray(level);
    let saturation = mood.saturation.clamp(0.0, 1.0);
    if saturation < 0.999 {
        painter.image(wall.gray.id(), dest, uv, tint);
    }
    if saturation > 0.001 {
        painter.image(wall.color.id(), dest, uv, tint.gamma_multiply(saturation));
    }
    if mood.brightness > 1.0 {
        let lift = ((mood.brightness - 1.0) * 0.45 * 255.0).round() as u8;
        painter.rect_filled(
            dest,
            CornerRadius::ZERO,
            Color32::from_rgba_premultiplied(lift, lift, lift, 0),
        );
    }
}

fn paint_backdrop(ui: &egui::Ui, window: egui::Rect, panel: egui::Rect, mood: Mood) {
    let ctx = ui.ctx().clone();
    let mood = mood.eased(&ctx);
    let drift = motion::drift(&ctx);
    let painter = ui.painter().with_clip_rect(window);
    painter.rect_filled(window, CornerRadius::ZERO, BG);

    if let Some(wall) = wallpaper(&ctx, false) {
        let uv = cover_uv(wall.color.size_vec2(), window, WALL_FOCUS);
        paint_wall(&painter, &wall, wall_rect(window, 1.0, drift), uv, mood);
    }
    let scrim = [(0.0, scrim_color(SCRIM_TOP)), (1.0, scrim_color(SCRIM_BOTTOM))];
    gradient(&painter, window, true, &scrim);

    let pane = painter.with_clip_rect(panel);
    if let Some(wall) = wallpaper(&ctx, true) {
        let uv = cover_uv(wall.color.size_vec2(), window, WALL_FOCUS);
        paint_wall(&pane, &wall, wall_rect(window, 1.08, drift), uv, mood);
        gradient(&pane, panel, true, &scrim);
    }
    gradient(
        &pane,
        panel,
        false,
        &[
            (0.0, with_alpha(PANEL_TINT, 0.28)),
            (0.26, with_alpha(PANEL_TINT, 0.82)),
            (1.0, with_alpha(PANEL_TINT, 0.88)),
        ],
    );
    gradient(
        &pane,
        egui::Rect::from_min_max(panel.min, egui::pos2(panel.min.x + 1.0, panel.max.y)),
        true,
        &[
            (0.0, Color32::TRANSPARENT),
            (0.16, tx(0.18)),
            (0.84, tx(0.18)),
            (1.0, Color32::TRANSPARENT),
        ],
    );
    if mood.dim > 0.001 {
        painter.rect_filled(window, CornerRadius::ZERO, scrim_color(mood.dim));
    }
}

pub fn drag_strip(ui: &mut egui::Ui, ctx: &egui::Context, rect: egui::Rect, id: &'static str) {
    let response = ui.interact(rect, egui::Id::new(id), egui::Sense::DRAG);
    if response.drag_started() {
        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}

fn titlebar_button_rect(window: egui::Rect, slot: f32) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(window.max.x - 10.0 - 30.0 * (slot + 1.0), window.min.y + 9.0),
        egui::vec2(30.0, 24.0),
    )
}

fn titlebar_button(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    slot_rect: egui::Rect,
    label: &str,
    enabled: bool,
    hover_fill: Color32,
    draw_icon: impl FnOnce(&egui::Painter, egui::Pos2, egui::Stroke),
) -> bool {
    let id = format!("titlebar-{}", label.to_lowercase());
    let response = ui.interact(
        slot_rect,
        egui::Id::new(&id),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let presence = motion::tween(
        ctx,
        egui::Id::new(id + "-presence"),
        if enabled { 1.0 } else { 0.3 },
        Spec::new(300, Curve::Ease),
    );
    let mut painter = ui.painter().clone();
    painter.multiply_opacity(presence);
    let icon = if enabled && response.hovered() {
        painter.rect_filled(slot_rect, CornerRadius::same(5), hover_fill);
        Color32::WHITE
    } else {
        tx(0.55)
    };
    draw_icon(&painter, slot_rect.center(), egui::Stroke::new(1.4, icon));
    focus_ring(&painter, slot_rect, 5, response.has_focus());
    enabled && response.clicked()
}

pub fn minimize_button(ui: &mut egui::Ui, ctx: &egui::Context, window: egui::Rect) {
    let hover = Color32::from_white_alpha(18);
    let bar = |painter: &egui::Painter, c: egui::Pos2, stroke| {
        painter.line_segment([egui::pos2(c.x - 4.5, c.y), egui::pos2(c.x + 4.5, c.y)], stroke);
    };
    if titlebar_button(ui, ctx, titlebar_button_rect(window, 1.0), "Minimize", true, hover, bar) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }
}

pub fn close_button(ui: &mut egui::Ui, ctx: &egui::Context, window: egui::Rect, enabled: bool) {
    let hover = Color32::from_rgba_unmultiplied(0xe0, 0x65, 0x5f, 190);
    let cross = |painter: &egui::Painter, c: egui::Pos2, stroke| {
        let r = 4.0;
        painter.line_segment([egui::pos2(c.x - r, c.y - r), egui::pos2(c.x + r, c.y + r)], stroke);
        painter.line_segment([egui::pos2(c.x - r, c.y + r), egui::pos2(c.x + r, c.y - r)], stroke);
    };
    if titlebar_button(ui, ctx, titlebar_button_rect(window, 0.0), "Close", enabled, hover, cross) {
        close(ctx);
    }
}

const CLOSE_NOW: &str = "peebify-close-now";

pub fn close(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(CLOSE_NOW), true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
}

fn hold_close(ctx: &egui::Context, closable: bool) {
    let release = ctx.data(|d| d.get_temp::<bool>(egui::Id::new(CLOSE_NOW))) == Some(true);
    if ctx.input(|i| i.viewport().close_requested()) && !release && !closable {
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
    }
}

const STEP_ROW: f32 = 17.0;
const STEP_LINK: f32 = 16.0;
const DOT: f32 = 7.0;
const STEPS: [(Step, &str); 3] = [
    (Step::Setup, "Setup"),
    (Step::Install, "Install"),
    (Step::Ready, "Ready"),
];

// ------------ Step Rail And Layouts ------------
// The left rail with the animated Setup, Install and Ready stepper, the error tag and the logo badge, and the split
// layout built around it.
fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

fn paint_label(painter: &egui::Painter, pos: egui::Pos2, label: &str, color: Color32) {
    let galley = painter.layout_no_wrap(
        label.to_owned(),
        egui::FontId::new(text::MD, egui::FontFamily::Proportional),
        color,
    );
    let y = pos.y + (STEP_ROW - galley.size().y) / 2.0;
    painter.galley(egui::pos2(pos.x + DOT + 11.0, y), galley, color);
}

fn paint_stepper(painter: &egui::Painter, x: f32, y: f32, step: Step, progress: f32) {
    let ctx = painter.ctx().clone();
    let id = egui::Id::new("peebify-stepper");
    let dot_fade = Spec::new(260, Curve::Ease).after(120);
    let dot_pop = Spec::new(520, Curve::Spring).after(120);
    let grow = Spec::new(420, Curve::Out);
    let fills = [
        motion::tween(&ctx, id.with("c1"), motion::flag(step >= Step::Install), grow),
        match step {
            Step::Install => {
                motion::tween(&ctx, id.with("c2"), progress, Spec::new(160, Curve::Linear))
            }
            Step::Ready => motion::tween(&ctx, id.with("c2"), 1.0, grow),
            Step::Setup => motion::tween(&ctx, id.with("c2"), 0.0, grow),
        },
    ];

    let mut row_y = y;
    for (i, (this, label)) in STEPS.into_iter().enumerate() {
        let on = motion::flag(this == step);
        let center = egui::pos2(x + DOT / 2.0, row_y + STEP_ROW / 2.0);
        painter.circle_stroke(center, DOT / 2.0 - 0.5, egui::Stroke::new(1.0, tx(0.30)));
        let fade = motion::tween(&ctx, id.with(("dot", i)), on, dot_fade);
        let pop = motion::tween(&ctx, id.with(("pop", i)), on, dot_pop);
        let scale = if motion::reduced() { 1.0 } else { 0.2 + 0.8 * pop };
        if fade > 0.001 {
            painter.circle_filled(
                center,
                (DOT / 2.0 * scale).max(0.0),
                Color32::WHITE.gamma_multiply(fade.clamp(0.0, 1.0)),
            );
        }
        let lit = motion::tween(&ctx, id.with(("label", i)), on, Spec::new(400, Curve::Ease));
        paint_label(painter, egui::pos2(x, row_y), label, lerp_color(tx(0.40), Color32::WHITE, lit));
        row_y += STEP_ROW;
        if let Some(fill) = fills.get(i) {
            let link = egui::Rect::from_min_size(egui::pos2(x + 3.0, row_y), egui::vec2(1.0, STEP_LINK));
            painter.rect_filled(link, CornerRadius::ZERO, tx(0.14));
            if *fill > 0.001 {
                let mut lit = link;
                lit.set_height(STEP_LINK * fill.clamp(0.0, 1.0));
                painter.rect_filled(lit, CornerRadius::ZERO, tx(0.55));
            }
            row_y += STEP_LINK;
        }
    }
}

fn badge_pop(ctx: &egui::Context, id: egui::Id, show: bool, delay: u32) -> (f32, f32, f32) {
    let on = motion::flag(show);
    let fade = if show {
        Spec::new(220, Curve::Ease).after(delay)
    } else {
        Spec::new(160, Curve::Ease)
    };
    let pop = if show {
        Spec::new(600, Curve::Spring).after(delay)
    } else {
        Spec::new(0, Curve::Linear).after(180)
    };
    let stroke = if show {
        Spec::new(420, Curve::InOut).after(delay + 220)
    } else {
        Spec::new(0, Curve::Linear).after(180)
    };
    (
        motion::tween(ctx, id.with("fade"), on, fade).clamp(0.0, 1.0),
        if motion::reduced() {
            1.0
        } else {
            0.4 + 0.6 * motion::tween(ctx, id.with("pop"), on, pop)
        },
        motion::tween(ctx, id.with("stroke"), on, stroke).clamp(0.0, 1.0),
    )
}

fn partial_path(points: &[egui::Pos2], fraction: f32) -> Vec<egui::Pos2> {
    let total: f32 = points.windows(2).map(|w| w[0].distance(w[1])).sum();
    let mut left = total * fraction.clamp(0.0, 1.0);
    let mut out = vec![points[0]];
    for w in points.windows(2) {
        let len = w[0].distance(w[1]);
        if left >= len {
            out.push(w[1]);
            left -= len;
        } else {
            if left > 0.0 {
                out.push(w[0] + (w[1] - w[0]) * (left / len));
            }
            break;
        }
    }
    out
}

fn paint_badge(painter: &egui::Painter, center: egui::Pos2, badge: Badge) {
    let ctx = painter.ctx().clone();
    let id = egui::Id::new("peebify-badge");
    let ring = Color32::from_rgb(14, 15, 24);
    let icon = |x: f32, y: f32, scale: f32| center + egui::vec2(x - 12.0, y - 12.0) * (10.0 / 24.0) * scale;

    let (fade, scale, stroke) = badge_pop(&ctx, id.with("ok"), badge == Badge::Done, 360);
    if fade > 0.001 {
        let mut p = painter.clone();
        p.multiply_opacity(fade);
        p.circle_filled(center, 10.0 * scale, ring);
        p.circle_filled(center, 8.0 * scale, Color32::WHITE);
        let check = [icon(20.0, 6.0, scale), icon(9.0, 17.0, scale), icon(4.0, 12.0, scale)];
        let drawn = partial_path(&check, stroke);
        if drawn.len() > 1 {
            p.add(egui::Shape::line(drawn, egui::Stroke::new(1.6 * scale, INK)));
        }
    }

    let (fade, scale, _) = badge_pop(&ctx, id.with("err"), badge == Badge::Failed, 200);
    if fade > 0.001 {
        let mut p = painter.clone();
        p.multiply_opacity(fade);
        p.circle_filled(center, 10.0 * scale, ring);
        p.circle_filled(center, 8.0 * scale, DANGER);
        let stroke = egui::Stroke::new(1.6 * scale, Color32::WHITE);
        p.line_segment([icon(12.0, 5.5, scale), icon(12.0, 13.0, scale)], stroke);
        p.circle_filled(icon(12.0, 18.5, scale), 0.95 * scale, Color32::WHITE);
    }
}

fn paint_rail(ui: &egui::Ui, rail_rect: egui::Rect, rail: &Rail) {
    let ctx = ui.ctx().clone();
    let x = rail_rect.min.x + RAIL_PAD_X;
    let y = rail_rect.min.y + RAIL_PAD_TOP;

    let painter = ui.painter().clone();
    if rail.error {
        painter.circle_filled(egui::pos2(x + DOT / 2.0, y + STEP_ROW / 2.0), DOT / 2.0, DANGER);
        paint_label(&painter, egui::pos2(x, y), "Error", DANGER);
    } else if let Some(step) = rail.step {
        paint_stepper(&painter, x, y, step, rail.progress.clamp(0.0, 1.0));
    }

    const LOGO: f32 = 34.0;
    const NAME_H: f32 = 21.0;
    const IDENT_H: f32 = 15.0;
    let block_h = LOGO + 11.0 + NAME_H + IDENT_H;
    let top = rail_rect.max.y - RAIL_PAD_BOTTOM - block_h;

    let logo_rect = egui::Rect::from_min_size(egui::pos2(x, top), egui::vec2(LOGO, LOGO));
    if let Some(logo) = rail.logo {
        let muted = motion::tween(
            &ctx,
            egui::Id::new("peebify-logo-muted"),
            motion::flag(rail.muted),
            Spec::new(700, Curve::Out),
        );
        let level = (255.0 * (1.0 - 0.15 * muted)).round() as u8;
        let tint = Color32::from_gray(level);
        let radius = CornerRadius::same(9);
        let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
        painter.add(
            egui::epaint::RectShape::filled(logo_rect, radius, tint)
                .with_texture(logo.id(), uv),
        );
        if muted > 0.001 {
            if let Some(gray) = gray_logo(&ctx) {
                painter.add(
                    egui::epaint::RectShape::filled(logo_rect, radius, tint.gamma_multiply(0.8 * muted))
                        .with_texture(gray.id(), uv),
                );
            }
        }
    }
    paint_badge(&painter, logo_rect.max - egui::vec2(3.0, 3.0), rail.badge);

    let name = painter.layout_no_wrap(
        crate::consts::PRODUCT_NAME.to_owned(),
        semibold_font(15.5),
        Color32::WHITE,
    );
    painter.galley(egui::pos2(x, top + LOGO + 11.0), name, Color32::WHITE);

    let ident_color = tx(0.56);
    let ident = painter.layout_no_wrap(
        rail.identity.clone(),
        egui::FontId::new(text::SM, egui::FontFamily::Proportional),
        ident_color,
    );
    painter.galley(egui::pos2(x, top + LOGO + 11.0 + NAME_H), ident, ident_color);
}

pub fn split(
    root: &mut egui::Ui,
    mood: Mood,
    closable: bool,
    rail: Rail,
    body: impl FnOnce(&mut egui::Ui),
) {
    let ctx = &root.ctx().clone();
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(root, |ui| {
            let rect = ui.max_rect();
            let panel =
                egui::Rect::from_min_max(egui::pos2(rect.min.x + RAIL_W, rect.min.y), rect.max);
            paint_backdrop(ui, rect, panel, mood);

            let rail_rect =
                egui::Rect::from_min_max(rect.min, egui::pos2(rect.min.x + RAIL_W, rect.max.y));
            drag_strip(ui, ctx, rail_rect, "rail-drag");
            drag_strip(
                ui,
                ctx,
                egui::Rect::from_min_size(panel.min, egui::vec2(panel.width(), 42.0)),
                "titlebar-drag",
            );
            minimize_button(ui, ctx, rect);
            close_button(ui, ctx, rect, closable);

            paint_rail(ui, rail_rect, &rail);

            let inner = egui::Rect::from_min_max(
                panel.min + egui::vec2(PANEL_PAD_X, PANEL_PAD_TOP),
                panel.max - egui::vec2(PANEL_PAD_X, PANEL_PAD_BOTTOM),
            );
            let mut body_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(inner)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            body_ui.spacing_mut().item_spacing.y = 0.0;
            body(&mut body_ui);
            hold_close(ctx, closable);
        });
}

// ------------ Shared Widgets ------------
// Text, cards, checkboxes, the folder field, the buttons and the progress bar.
pub fn footer(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let rect = ui.max_rect();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::bottom_up(egui::Align::Min)),
    );
    child.spacing_mut().item_spacing.y = 0.0;
    add(&mut child);
}

pub fn action_row(ui: &mut egui::Ui, right_aligned: bool, add: impl FnOnce(&mut egui::Ui)) {
    let layout = if right_aligned {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 38.0), layout, add);
}

pub fn wrapped_label(body: &str, size: f32, color: Color32, leading: f32) -> egui::Label {
    egui::Label::new(
        RichText::new(body)
            .size(size)
            .color(color)
            .line_height(Some(size * leading)),
    )
    .wrap()
}

pub fn paragraph(ui: &mut egui::Ui, body: &str, size: f32, color: Color32, max_width: f32) {
    let width = max_width.min(ui.available_width());
    ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
        ui.add(wrapped_label(body, size, color, 1.55));
    });
}

pub fn meta_label(ui: &mut egui::Ui, label: &str) {
    ui.label(
        RichText::new(label.to_uppercase())
            .size(text::MICRO)
            .color(tx(0.55))
            .extra_letter_spacing(text::MICRO * 0.04),
    );
}

pub fn hairline(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    gradient(
        ui.painter(),
        rect,
        false,
        &[
            (0.0, Color32::TRANSPARENT),
            (0.12, tx(0.14)),
            (0.88, tx(0.14)),
            (1.0, Color32::TRANSPARENT),
        ],
    );
}

pub fn bullet_row(ui: &mut egui::Ui, color: Color32, body: &str) {
    let width = ui.available_width();
    ui.horizontal_top(|row| {
        let line = text::SM * 1.5;
        let (dot, _) = row.allocate_exact_size(egui::vec2(5.0, line), egui::Sense::hover());
        row.painter().circle_filled(
            egui::pos2(dot.min.x + 2.5, dot.min.y + line / 2.0),
            2.5,
            color,
        );
        row.add_space(9.0);
        row.allocate_ui(egui::vec2(width - 14.0, 0.0), |col| {
            col.add(wrapped_label(body, text::SM, color, 1.5));
        });
    });
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Notice,
    Danger,
    Plain,
}

impl Tone {
    fn colors(self) -> (Color32, Color32) {
        match self {
            Tone::Notice => (NOTICE.gamma_multiply(0.26), NOTICE.gamma_multiply(0.06)),
            Tone::Danger => (DANGER.gamma_multiply(0.24), DANGER.gamma_multiply(0.06)),
            Tone::Plain => (tx(0.12), tx(0.04)),
        }
    }
}

pub fn card<R>(ui: &mut egui::Ui, tone: Tone, pad: i8, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let (stroke, fill) = tone.colors();
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(pad))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        })
        .inner
}

pub fn detail_box(ui: &mut egui::Ui, tone: Tone, label: &str, body: &str, max_height: f32) {
    card(ui, tone, 12, |card| {
        meta_label(card, label);
        card.add_space(5.0);
        egui::ScrollArea::vertical()
            .max_height(max_height)
            .auto_shrink([false, true])
            .show(card, |text| {
                text.add(wrapped_label(body, text::SM, tx(0.82), 1.55).selectable(true));
            });
    });
}

pub fn text_field(ui: &mut egui::Ui, value: &mut String, size: egui::Vec2) -> egui::Response {
    const RADIUS: u8 = 8;
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let id = ui.make_persistent_id("peebify-text-field");
    let focused = ui.memory(|m| m.has_focus(id));
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(RADIUS), tx(0.05));
    painter.rect_stroke(
        rect,
        CornerRadius::same(RADIUS),
        egui::Stroke::new(1.0, if focused { tx(0.55) } else { tx(0.16) }),
        egui::StrokeKind::Inside,
    );

    let inner = rect.shrink2(egui::vec2(11.0, 1.0));
    let mut field_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    field_ui.add_sized(
        inner.size(),
        egui::TextEdit::singleline(value)
            .id(id)
            .frame(egui::Frame::NONE)
            .text_color(tx(0.85))
            .font(egui::FontId::new(text::MD, egui::FontFamily::Proportional))
            .vertical_align(egui::Align::Center)
            .margin(egui::Margin::ZERO),
    )
}

pub fn checkbox(ui: &mut egui::Ui, checked: &mut bool, label: &str) -> egui::Response {
    const BOX: f32 = 17.0;
    const GAP: f32 = 11.0;
    const RADIUS: u8 = 4;
    let font = egui::FontId::new(text::BASE, egui::FontFamily::Proportional);
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER));
    let size = egui::vec2(BOX + GAP + galley.size().x, BOX.max(galley.size().y));
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *checked = !*checked;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, *checked, label)
    });
    let ctx = ui.ctx().clone();
    let on = motion::flag(*checked);
    let hover = ctx.animate_bool_with_time(response.id, response.hovered(), 0.15);
    let fill = motion::tween(&ctx, response.id.with("fill"), on, Spec::new(160, Curve::Ease));
    let pop = motion::tween(&ctx, response.id.with("pop"), on, Spec::new(380, Curve::Spring));
    let draw = motion::tween(
        &ctx,
        response.id.with("draw"),
        on,
        if *checked {
            Spec::new(280, Curve::InOut).after(80)
        } else {
            Spec::new(120, Curve::Ease)
        },
    );

    let scale = if *checked && !motion::reduced() {
        0.9 + 0.1 * pop
    } else {
        1.0
    };
    let box_rect = egui::Rect::from_center_size(
        egui::pos2(rect.min.x + BOX / 2.0, rect.center().y),
        egui::vec2(BOX, BOX) * scale,
    );
    let painter = ui.painter();
    let idle_border = tx(0.32 + 0.23 * hover);
    let fill_color = lerp_color(Color32::from_white_alpha((14.0 * hover) as u8), Color32::WHITE, fill);
    painter.rect_filled(box_rect, CornerRadius::same(RADIUS), fill_color);
    painter.rect_stroke(
        box_rect,
        CornerRadius::same(RADIUS),
        egui::Stroke::new(1.0, lerp_color(idle_border, Color32::WHITE, fill)),
        egui::StrokeKind::Inside,
    );
    if draw > 0.001 {
        let origin = box_rect.center() - egui::vec2(5.5, 4.5) * scale;
        let at = |x: f32, y: f32| origin + egui::vec2(x, y) * scale;
        let points = partial_path(&[at(1.5, 4.6), at(4.5, 7.5), at(9.5, 1.5)], draw);
        if points.len() > 1 {
            painter.add(egui::Shape::line(points, egui::Stroke::new(2.0 * scale, INK)));
        }
    }
    let color = lerp_color(tx(0.90), Color32::WHITE, hover);
    painter.galley(
        egui::pos2(
            rect.min.x + BOX + GAP,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );
    focus_ring(painter, box_rect, RADIUS, response.has_focus());
    response
        .clone()
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response
}

pub fn sub_note(ui: &mut egui::Ui, body: &str) {
    ui.horizontal_top(|row| {
        row.add_space(28.0);
        let width = row.available_width();
        row.allocate_ui(egui::vec2(width, 0.0), |col| {
            col.add(wrapped_label(body, text::XS, tx(0.56), 1.5));
        });
    });
}

struct Pill {
    text: Color32,
    text_hover: Color32,
    fill: Color32,
    fill_hover: Color32,
    stroke: Color32,
    stroke_hover: Color32,
    pad: f32,
    height: f32,
    min_width: f32,
}

fn pill_button(ui: &mut egui::Ui, label: &str, pill: Pill) -> egui::Response {
    let font = semibold_font(text::BODY);
    let enabled = ui.is_enabled();
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER));
    let size = egui::vec2(
        (galley.size().x + pill.pad * 2.0).max(pill.min_width),
        pill.height,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));

    let t = ui
        .ctx()
        .animate_bool_with_time(response.id, enabled && response.hovered(), 0.15);
    let pressed = ui.ctx().animate_bool_with_time(
        response.id.with("press"),
        enabled && response.is_pointer_button_down_on(),
        0.12,
    );
    let rect = if motion::reduced() {
        rect
    } else {
        egui::Rect::from_center_size(rect.center(), rect.size() * (1.0 - 0.03 * pressed))
    };

    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), lerp_color(pill.fill, pill.fill_hover, t));
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        egui::Stroke::new(1.0, lerp_color(pill.stroke, pill.stroke_hover, t)),
        egui::StrokeKind::Inside,
    );
    let text_pos = rect.center() - galley.size() / 2.0;
    painter.galley(text_pos, galley, lerp_color(pill.text, pill.text_hover, t));
    focus_ring(painter, rect, 8, response.has_focus());

    if enabled {
        response
            .clone()
            .on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    response
}

fn focus_ring(painter: &egui::Painter, rect: egui::Rect, radius: u8, focused: bool) {
    if !focused {
        return;
    }
    painter.rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius + 2),
        egui::Stroke::new(1.5, tx(0.75)),
        egui::StrokeKind::Outside,
    );
}

fn secondary_pill(pad: f32, height: f32, min_width: f32) -> Pill {
    Pill {
        text: tx(0.78),
        text_hover: Color32::WHITE,
        fill: Color32::TRANSPARENT,
        fill_hover: tx(0.06),
        stroke: tx(0.20),
        stroke_hover: tx(0.34),
        pad,
        height,
        min_width,
    }
}

pub fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    if !ui.is_enabled() {
        return secondary_button(ui, label);
    }
    pill_button(
        ui,
        label,
        Pill {
            text: INK,
            text_hover: INK,
            fill: Color32::WHITE,
            fill_hover: tx(0.88),
            stroke: Color32::WHITE,
            stroke_hover: Color32::WHITE,
            pad: 20.0,
            height: 38.0,
            min_width: 0.0,
        },
    )
}

pub fn secondary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    pill_button(ui, label, secondary_pill(20.0, 38.0, 0.0))
}

pub fn wide_secondary_button(ui: &mut egui::Ui, label: &str, min_width: f32) -> egui::Response {
    pill_button(ui, label, secondary_pill(20.0, 38.0, min_width))
}

pub fn small_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    pill_button(ui, label, secondary_pill(15.0, 36.0, 0.0))
}

pub fn danger_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    pill_button(
        ui,
        label,
        Pill {
            text: DANGER_TEXT,
            text_hover: DANGER_TEXT,
            fill: DANGER.gamma_multiply(0.09),
            fill_hover: DANGER.gamma_multiply(0.18),
            stroke: DANGER.gamma_multiply(0.70),
            stroke_hover: DANGER.gamma_multiply(0.95),
            pad: 24.0,
            height: 38.0,
            min_width: 0.0,
        },
    )
}

pub fn link(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let font = egui::FontId::new(12.0, egui::FontFamily::Proportional);
    let color = tx(0.60);
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font, color));
    let (rect, response) =
        ui.allocate_exact_size(galley.size() + egui::vec2(0.0, 4.0), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, label));
    let hovered = response.hovered();
    let color = if hovered { tx(0.85) } else { color };
    let painter = ui.painter();
    painter.galley(rect.min, galley, color);
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.max.y - 1.0),
            egui::pos2(rect.max.x, rect.max.y - 1.0),
        ],
        egui::Stroke::new(1.0, if hovered { tx(0.55) } else { tx(0.28) }),
    );
    focus_ring(painter, rect, 2, response.has_focus());
    response
        .clone()
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response
}

pub const BAR_STEADY: Spec = Spec::new(160, Curve::Linear);

pub fn progress_bar(ui: &mut egui::Ui, id: egui::Id, fraction: f32, spec: Spec) {
    const HEIGHT: f32 = 6.0;
    const RADIUS: u8 = 3;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), HEIGHT),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, CornerRadius::same(RADIUS), tx(0.10));

    if fraction < 0.0 {
        let width = rect.width() * 0.3;
        let travel = rect.width() + width;
        let t = (ui.input(|i| i.time) % 1.4) as f32 / 1.4;
        let x = rect.min.x - width + travel * t;
        let band = egui::Rect::from_min_max(
            egui::pos2(x.max(rect.min.x), rect.min.y),
            egui::pos2((x + width).min(rect.max.x), rect.max.y),
        );
        if band.width() > 0.5 {
            ui.painter()
                .rect_filled(band, CornerRadius::same(RADIUS), Color32::WHITE);
        }
        ui.ctx().request_repaint();
        return;
    }

    let eased = motion::tween(ui.ctx(), id, fraction.clamp(0.0, 1.0), spec);
    let mut fill = rect;
    fill.set_width((rect.width() * eased).max(rect.width() * 0.015).max(HEIGHT));
    ui.painter()
        .rect_filled(fill, CornerRadius::same(RADIUS), Color32::WHITE);

    if motion::reduced() {
        return;
    }
    let band = fill.width() * 0.35;
    let t = (ui.input(|i| i.time) % 1.6) as f32 / 1.6;
    let x = fill.min.x + band * (-1.0 + 3.86 * Curve::Sine.at(t));
    let sheen = egui::Rect::from_min_size(egui::pos2(x, fill.min.y), egui::vec2(band, HEIGHT));
    let shade = |a: f32| with_alpha(INK, a);
    gradient(
        &ui.painter().with_clip_rect(fill.shrink2(egui::vec2(1.0, 0.0))),
        sheen,
        false,
        &[(0.0, shade(0.0)), (0.5, shade(0.22)), (1.0, shade(0.0))],
    );
    ui.ctx().request_repaint();
}

// ------------ Progress Pacing And Engine Thread ------------
// step_label folds the engine's phases into the few steps the progress screen names. Pacer holds each phase on screen
// for a moment so fast ones do not flicker past. spawn_engine runs the install or uninstall work on a thread, and
// friendly_error turns raw OS errors into plain advice.
pub fn step_label(phase: &str) -> Option<&str> {
    Some(match phase {
        "Verifying installer…" => return None,
        "Closing Peebify Launcher…" => "Closing the launcher…",
        "Copying files…" | "Writing uninstaller…" | "Installing files…" => "Copying files…",
        "Registering with Windows…" | "Creating shortcuts…" => "Creating shortcuts…",
        "Checking WebView2 runtime…" | "Checking Visual C++ runtime…" | "Finishing up…" => {
            "Finishing up…"
        }
        "Removing files…" | "Removing shortcuts and registry entries…" => "Removing files…",
        "Removing installed games…" => "Removing game files…",
        "Removing settings and data…" => "Removing settings & mods…",
        other => other,
    })
}

pub struct Pacer {
    phase: String,
    percent: f32,
    queue: std::collections::VecDeque<(String, f32)>,
    shown_at: std::time::Instant,
    finished: bool,
}

const PHASE_DWELL: std::time::Duration = std::time::Duration::from_millis(200);

impl Pacer {
    pub fn new(initial: impl Into<String>) -> Self {
        Self {
            phase: initial.into(),
            percent: 0.0,
            queue: std::collections::VecDeque::new(),
            shown_at: std::time::Instant::now(),
            finished: false,
        }
    }

    pub fn push(&mut self, phase: String, percent: f32) {
        let current = self.queue.back().map(|(p, _)| p).unwrap_or(&self.phase);
        if current == &phase {
            self.advance(percent);
            return;
        }
        self.queue.push_back((phase, percent));
    }

    pub fn advance(&mut self, percent: f32) {
        match self.queue.back_mut() {
            Some(last) => last.1 = percent,
            None => self.percent = percent,
        }
    }

    pub fn report(&mut self, phase: &str, percent: f32) {
        match step_label(phase) {
            Some(label) => self.push(label.to_string(), percent),
            None => self.advance(percent),
        }
    }

    pub fn finish(&mut self) {
        self.finish_as("Finishing up…");
    }

    pub fn finish_as(&mut self, closing: &str) {
        self.finished = true;
        let last = self.queue.back().map(|(_, p)| *p).unwrap_or(self.percent);
        if last < 100.0 {
            self.push(closing.to_string(), 100.0);
        }
        self.advance(100.0);
    }

    pub fn tick(&mut self) {
        while self.shown_at.elapsed() >= PHASE_DWELL {
            let Some((phase, percent)) = self.queue.pop_front() else {
                break;
            };
            self.phase = phase;
            self.percent = percent;
            self.shown_at = std::time::Instant::now();
        }
    }

    pub fn phase(&self) -> &str {
        &self.phase
    }

    pub fn percent(&self) -> f32 {
        self.percent
    }

    pub fn settled(&self) -> bool {
        self.finished && self.queue.is_empty() && self.shown_at.elapsed() >= PHASE_DWELL
    }
}

pub fn progress_readout(ui: &mut egui::Ui, label: &str, percent: f32, label_size: f32, pct_size: f32) {
    ui.horizontal(|row| {
        row.label(RichText::new(label).size(label_size).color(tx(0.85)));
        if percent >= 0.0 {
            row.with_layout(egui::Layout::right_to_left(egui::Align::Center), |row| {
                row.label(
                    RichText::new(format!("{}%", percent.round() as i32))
                        .size(pct_size)
                        .color(tx(0.55)),
                );
            });
        }
    });
}

pub fn progress_block(ui: &mut egui::Ui, id: &'static str, label: &str, percent: f32) {
    ui.scope(|g| {
        g.add_space(26.0);
        progress_readout(g, label, percent, text::BASE, 12.0);
        g.add_space(10.0);
        progress_bar(g, egui::Id::new(id), bar_fraction(percent), BAR_STEADY);
    });
}

pub fn warnings_card(ui: &mut egui::Ui, warnings: &[String], max_height: f32) {
    card(ui, Tone::Notice, 13, |card| {
        egui::ScrollArea::vertical()
            .max_height(max_height)
            .auto_shrink([false, true])
            .show(card, |list| {
                for (i, warning) in warnings.iter().enumerate() {
                    if i > 0 {
                        list.add_space(10.0);
                    }
                    bullet_row(list, NOTICE, warning);
                }
            });
    });
}

pub fn run_window(
    title: &str,
    size: [f32; 2],
    build: impl FnOnce(&egui::Context, std::rc::Rc<std::cell::Cell<i32>>) -> Box<dyn eframe::App> + 'static,
) -> eframe::Result<i32> {
    let options = eframe::NativeOptions {
        viewport: viewport(size),
        centered: true,
        ..Default::default()
    };
    let outcome = std::rc::Rc::new(std::cell::Cell::new(crate::exit::OK));
    let slot = outcome.clone();
    prewarm();
    eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            Ok(build(&cc.egui_ctx, slot))
        }),
    )?;
    Ok(outcome.get())
}

pub fn spawn_engine(
    work: impl FnOnce(&crate::msg::EventSink) -> Result<(), crate::msg::EngineError> + Send + 'static,
) -> std::sync::mpsc::Receiver<crate::msg::EngineEvent> {
    use crate::msg::{EngineError, EngineEvent};
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = work(&tx);
        let _ = match result {
            Ok(()) => tx.send(EngineEvent::Finished),
            Err(EngineError::Cancelled) => tx.send(EngineEvent::Cancelled),
            Err(EngineError::Failed(e)) => tx.send(EngineEvent::Failed(e)),
        };
    });
    rx
}

pub fn next_event(
    rx: &mut Option<std::sync::mpsc::Receiver<crate::msg::EngineEvent>>,
    pacer: &mut Pacer,
) -> Option<crate::msg::EngineEvent> {
    use crate::msg::EngineEvent;
    loop {
        let event = rx.as_ref()?.try_recv().ok()?;
        match event {
            EngineEvent::Status { phase, percent } => pacer.report(&phase, percent),
            EngineEvent::Finished | EngineEvent::Cancelled | EngineEvent::Failed(_) => {
                *rx = None;
                return Some(event);
            }
            other => return Some(other),
        }
    }
}

fn has_os_error(lower: &str, code: u32) -> bool {
    lower.match_indices("os error ").any(|(at, needle)| {
        let digits: String = lower[at + needle.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse::<u32>() == Ok(code)
    })
}

pub fn friendly_error(raw: &str) -> &'static str {
    let lower = raw.to_lowercase();
    if has_os_error(&lower, 5) || lower.contains("access is denied") {
        "Windows or security software blocked a file setup needed. Close Peebify \
         Launcher and any running game, allow setup in your antivirus, or choose a \
         different folder, then try again."
    } else if has_os_error(&lower, 112) || lower.contains("not enough space") {
        "The drive ran out of space partway through. \
         Free some room, or install to another drive, and try again."
    } else if has_os_error(&lower, 32) || lower.contains("being used by another process") {
        "A file setup needed was in use. Close Peebify Launcher and any running \
         game, then try again."
    } else if lower.contains("checksum") || lower.contains("corrupt") {
        "The download is damaged. Download setup again from peebify.net. \
         A partial download is the usual cause."
    } else {
        ""
    }
}

pub fn bar_fraction(percent: f32) -> f32 {
    if percent < 0.0 {
        percent
    } else {
        percent / 100.0
    }
}

pub fn log_dir() -> Option<std::path::PathBuf> {
    crate::consts::user_data_dir().map(|d| d.join("logs"))
}

#[derive(Default)]
pub struct Copied {
    at: Option<std::time::Instant>,
}

impl Copied {
    pub fn mark(&mut self) {
        self.at = Some(std::time::Instant::now());
    }

    pub fn label(&self, ctx: &egui::Context) -> &'static str {
        const SHOWN: std::time::Duration = std::time::Duration::from_millis(1400);
        match self.at {
            Some(at) if at.elapsed() < SHOWN => {
                ctx.request_repaint_after(SHOWN - at.elapsed());
                "Copied"
            }
            _ => "Copy details",
        }
    }
}

pub enum Failure {
    Retry,
    Close,
}

pub fn failure_page(
    ui: &mut egui::Ui,
    copied: &mut Copied,
    err: &str,
    title: &str,
    note: Option<&str>,
    retry: bool,
) -> Option<Failure> {
    page_title(ui, 12.0, title, 26.0);
    let friendly = friendly_error(err);
    ui.scope(|g| {
        g.add_space(18.0);
        detail_box(g, Tone::Danger, "Details", err, 170.0);
        if !friendly.is_empty() {
            g.add_space(10.0);
            paragraph(g, friendly, text::XS, tx(0.62), 400.0);
        }
        if let Some(note) = note {
            g.add_space(10.0);
            paragraph(g, note, text::XS, DANGER_TEXT, 400.0);
        }
    });
    failure_actions(ui, copied, err, retry)
}

pub fn failure_actions(
    ui: &mut egui::Ui,
    copied: &mut Copied,
    err: &str,
    retry: bool,
) -> Option<Failure> {
    let ctx = ui.ctx().clone();
    let copy_label = copied.label(&ctx);
    let (mut copy, mut open_log, mut again, mut close) = (false, false, false, false);
    footer(ui, |foot| {
        foot.scope(|foot| {
            action_row_split(
                foot,
                |left| {
                    copy = wide_secondary_button(left, copy_label, 116.0).clicked();
                    left.add_space(14.0);
                    open_log = link(left, "Open log").clicked();
                },
                |right| {
                    if retry {
                        again = primary_button(right, "Retry").clicked();
                        right.add_space(10.0);
                        close = secondary_button(right, "Close").clicked();
                    } else {
                        close = primary_button(right, "Close").clicked();
                    }
                },
            );
        });
    });
    if copy {
        ctx.copy_text(err.to_string());
        copied.mark();
    } else if open_log {
        open_log_folder();
    } else if again {
        return Some(Failure::Retry);
    } else if close {
        return Some(Failure::Close);
    }
    None
}

pub fn action_row_split(
    ui: &mut egui::Ui,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) {
    action_row(ui, false, |row| {
        left(row);
        row.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
}

pub fn open_log_folder() {
    if let Some(dir) = log_dir() {
        crate::win::open_folder(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::{friendly_error, step_label, Pacer};

    #[test]
    fn access_denied_never_suggests_administrator() {
        for raw in ["Access is denied. (os error 5)", "rename failed: ACCESS IS DENIED"] {
            let msg = friendly_error(raw);
            assert!(msg.contains("security software"));
            assert!(!msg.to_lowercase().contains("administrator"));
        }
    }

    #[test]
    fn os_error_codes_match_exactly() {
        for raw in [
            "copy failed: The network path was not found. (os error 53)",
            "read failed: An unexpected network error occurred. (os error 59)",
            "write failed (os error 320)",
            "write failed (os error 1120)",
        ] {
            assert_eq!(friendly_error(raw), "", "{raw}");
        }
        assert!(friendly_error("write failed (os error 112)").contains("ran out of space"));
        assert!(friendly_error("rename failed (os error 32)").contains("in use"));
        assert!(friendly_error("os error 5").contains("security software"));
    }

    #[test]
    fn finish_does_not_follow_done_with_finishing_up() {
        let mut pacer = Pacer::new("Preparing…");
        pacer.push("Done".to_string(), 100.0);
        pacer.finish();
        assert_eq!(pacer.queue.back().map(|(p, _)| p.as_str()), Some("Done"));

        let mut pacer = Pacer::new("Preparing…");
        pacer.push("Copying…".to_string(), 60.0);
        pacer.finish();
        assert_eq!(
            pacer.queue.back().map(|(p, pct)| (p.as_str(), *pct)),
            Some(("Finishing up…", 100.0))
        );
    }

    #[test]
    fn engine_phases_collapse_into_the_four_install_steps() {
        let mut pacer = Pacer::new("Copying files…");
        for (phase, pct) in [
            ("Verifying installer…", 2.0),
            ("Copying files…", 30.0),
            ("Writing uninstaller…", 68.0),
            ("Installing files…", 72.0),
            ("Registering with Windows…", 78.0),
            ("Creating shortcuts…", 82.0),
            ("Checking WebView2 runtime…", 86.0),
            ("Finishing up…", 98.0),
        ] {
            pacer.report(phase, pct);
        }
        let labels: Vec<&str> = pacer.queue.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(labels, ["Creating shortcuts…", "Finishing up…"]);
        assert_eq!(pacer.percent(), 72.0);
    }

    #[test]
    fn unknown_phases_pass_through_and_verification_is_silent() {
        assert_eq!(step_label("Downloading WebView2…"), Some("Downloading WebView2…"));
        assert_eq!(step_label("Verifying installer…"), None);
        assert_eq!(step_label("Removing installed games…"), Some("Removing game files…"));
    }
}
