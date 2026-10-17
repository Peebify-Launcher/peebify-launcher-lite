// ------------ Install Wizard ------------
// The install window: Welcome, Options (folder, shortcut, Visual C++ runtime, launch afterwards), Progress, then Done,
// a warnings page or Failed with Retry. The chosen folder is checked in the background and the install itself runs on
// a worker thread. Screens switch instantly.

use std::path::PathBuf;

use eframe::egui::{self, RichText};

use crate::consts;
use crate::install::{human_bytes, launch_app, perform_install, required_bytes, InstallOptions};
use crate::msg::{Cancel, EngineEvent};
use crate::paths;
use crate::payload::Payload;
use crate::ui;

pub fn run(payload: Payload) -> eframe::Result<i32> {
    ui::run_window("Peebify Launcher Setup", ui::WINDOW, move |ctx, outcome| {
        Box::new(WizardApp::new(ctx, payload, outcome))
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Welcome,
    Options,
    Progress,
    Done,
}

#[derive(Clone, Copy, PartialEq, Hash, Debug)]
enum Screen {
    Welcome,
    Options,
    Progress,
    Done,
    Warnings,
    Failed,
}

struct WizardApp {
    payload: Payload,
    logo: Option<egui::TextureHandle>,
    page: Page,
    install_dir: String,
    desktop_shortcut: bool,
    launch_after: bool,
    vc_redist_missing: bool,
    install_vc_redist: bool,
    rx: Option<std::sync::mpsc::Receiver<EngineEvent>>,
    cancel: Cancel,
    cancelling: bool,
    committed: bool,
    pacer: ui::Pacer,
    warnings: Vec<String>,
    error: Option<String>,
    launcher_running: bool,
    running_checked_at: Option<std::time::Instant>,
    chrome: ui::Chrome,
    copied: ui::Copied,
    launched_at: Option<std::time::Instant>,
    stay_open: bool,
    webview2_missing: bool,
    registered: Option<PathBuf>,
    target_check: Option<(String, std::time::Instant, TargetCheck)>,
    target_job: Option<(String, std::sync::mpsc::Receiver<TargetCheck>)>,
    target_stale_since: Option<std::time::Instant>,
    outcome: std::rc::Rc<std::cell::Cell<i32>>,
}

struct TargetCheck {
    refusal: Option<&'static str>,
    free: Option<u64>,
    needed: u64,
    replacing: bool,
    foreign: bool,
    elsewhere: Option<PathBuf>,
}

impl TargetCheck {
    fn blocks_install(&self) -> bool {
        self.refusal.is_some() || self.free.is_some_and(|free| free < self.needed)
    }

    fn short_of_space(&self) -> Option<String> {
        match self.free {
            Some(free) if free < self.needed => Some(format!(
                "Needs about {}, only {} free on this drive.",
                human_bytes(self.needed),
                human_bytes(free)
            )),
            _ => None,
        }
    }

    fn size_line(&self) -> Option<String> {
        if self.refusal.is_some() {
            return None;
        }
        let free = self.free.map(human_bytes)?;
        Some(format!("{} · {free} free", human_bytes(self.needed)))
    }
}

fn compute_target_check(
    current: &str,
    estimated_size_kb: u64,
    registered: Option<&std::path::Path>,
) -> TargetCheck {
    let dir = PathBuf::from(current);
    let refusal = if current.is_empty() {
        Some("Enter a folder to install into.")
    } else {
        paths::validate_install_dir(&dir).err().map(|r| r.message())
    }
    .or_else(|| {
        (!dir.ancestors().any(|a| a.exists()))
            .then_some("That drive isn't available. Pick a folder on a connected drive.")
    })
    .or_else(|| {
        let collides =
            !crate::swap::foreign_collisions(&dir, &crate::swap::expected_top_names())
                .is_empty();
        collides.then_some(
            "This folder already has its own resources or icons, which setup would \
             replace. Choose an empty folder, or add \"Peebify Launcher\" to the end \
             of the path.",
        )
    });
    let usable = refusal.is_none();
    TargetCheck {
        refusal,
        free: if usable {
            crate::win::free_space_bytes(&dir)
        } else {
            None
        },
        needed: required_bytes(estimated_size_kb),
        replacing: usable && dir.join(consts::INSTALL_MANIFEST_NAME).exists(),
        foreign: usable && paths::is_foreign_non_empty(&dir),
        elsewhere: registered
            .filter(|registered| {
                usable && registered.is_dir() && !paths::same_path(registered, &dir)
            })
            .map(std::path::Path::to_path_buf),
    }
}

fn install_allowed(
    check: Option<&(String, std::time::Instant, TargetCheck)>,
    current: &str,
) -> bool {
    check.is_some_and(|(seen, _, check)| seen == current && !check.blocks_install())
}

fn existing_ancestor(path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    PathBuf::from(path)
        .ancestors()
        .find(|a| !a.as_os_str().is_empty() && a.is_dir())
        .map(std::path::Path::to_path_buf)
}

const TARGET_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);
const TARGET_STALE_GRACE: std::time::Duration = std::time::Duration::from_millis(400);
const CHECKING_FOLDER: &str = "Checking folder…";
const AUTO_CLOSE: std::time::Duration = std::time::Duration::from_secs(3);
const REPLACING_NOTICE: &str =
    "Seems an install already exists! We'll replace only the files needed for this version.";

fn clean_target(raw: &str) -> String {
    raw.trim().trim_matches('"').trim().to_string()
}

fn initial_install_dir(registered: Option<PathBuf>) -> PathBuf {
    registered
        .filter(|dir| paths::validate_install_dir(dir).is_ok())
        .unwrap_or_else(consts::default_install_dir)
}

fn elsewhere_notice(registered: &std::path::Path) -> String {
    format!(
        "{} is already installed in {}. Installing here leaves that copy in place. \
         Run its {} to remove it.",
        consts::PRODUCT_NAME,
        registered.display(),
        consts::UNINSTALLER_NAME
    )
}

fn path_line(ui_: &mut egui::Ui, path: &str) {
    ui_.add(
        egui::Label::new(RichText::new(path).size(ui::text::MD).color(ui::tx(0.80))).truncate(),
    )
    .on_hover_text(path);
}

impl WizardApp {
    fn new(
        ctx: &egui::Context,
        payload: Payload,
        outcome: std::rc::Rc<std::cell::Cell<i32>>,
    ) -> Self {
        let vc_redist_missing = !crate::prereqs::vcredist::is_installed();
        let registered = crate::win::registered_install_location();
        Self {
            logo: ui::load_logo(ctx),
            install_dir: initial_install_dir(registered.clone())
                .display()
                .to_string(),
            desktop_shortcut: true,
            launch_after: true,
            vc_redist_missing,
            install_vc_redist: vc_redist_missing,
            page: Page::Welcome,
            rx: None,
            cancel: Cancel::default(),
            cancelling: false,
            committed: false,
            pacer: ui::Pacer::new(""),
            warnings: Vec::new(),
            error: None,
            launcher_running: false,
            running_checked_at: None,
            chrome: ui::Chrome::default(),
            copied: ui::Copied::default(),
            launched_at: None,
            stay_open: false,
            webview2_missing: false,
            registered,
            target_check: None,
            target_job: None,
            target_stale_since: None,
            outcome,
            payload,
        }
    }

    fn screen(&self) -> Screen {
        match self.page {
            Page::Welcome => Screen::Welcome,
            Page::Options => Screen::Options,
            Page::Progress => Screen::Progress,
            Page::Done if self.error.is_some() => Screen::Failed,
            Page::Done if !self.warnings.is_empty() => Screen::Warnings,
            Page::Done => Screen::Done,
        }
    }

    fn enter_pressed(ui_: &egui::Ui) -> bool {
        ui_.input(|i| i.key_pressed(egui::Key::Enter)) && ui_.memory(|m| m.focused().is_none())
    }

    fn launcher_running(&mut self) -> bool {
        let stale = self
            .running_checked_at
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(2))
            .unwrap_or(true);
        if stale {
            self.launcher_running =
                !crate::win::pids_by_exe_name(&self.payload.manifest.main_binary).is_empty();
            self.running_checked_at = Some(std::time::Instant::now());
        }
        self.launcher_running
    }

    fn target_path(&self) -> String {
        clean_target(&self.install_dir)
    }

    fn poll_target_check(&mut self, ctx: &egui::Context) {
        let received = self.target_job.as_ref().map(|(_, rx)| rx.try_recv());
        match received {
            Some(Ok(check)) => {
                if let Some((path, _)) = self.target_job.take() {
                    self.target_check = Some((path, std::time::Instant::now(), check));
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => self.target_job = None,
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
        }

        let current = self.target_path();
        let (fresh, due) = match &self.target_check {
            Some((seen, at, _)) => (
                seen == &current,
                seen != &current || at.elapsed() >= TARGET_RECHECK,
            ),
            None => (false, true),
        };
        if fresh {
            self.target_stale_since = None;
        } else if self.target_stale_since.is_none() {
            self.target_stale_since = Some(std::time::Instant::now());
        }
        if due && self.target_job.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            let path = current.clone();
            let estimated_size_kb = self.payload.manifest.estimated_size_kb;
            let registered = self.registered.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let check = compute_target_check(&path, estimated_size_kb, registered.as_deref());
                let _ = tx.send(check);
                ctx.request_repaint();
            });
            self.target_job = Some((current, rx));
        }
        if !fresh {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn install_ready(&self) -> bool {
        install_allowed(self.target_check.as_ref(), &self.target_path())
    }

    fn shown_check(&self) -> Option<&TargetCheck> {
        let (seen, _, check) = self.target_check.as_ref()?;
        let recent = self
            .target_stale_since
            .is_none_or(|since| since.elapsed() < TARGET_STALE_GRACE);
        (seen == &self.target_path() || recent).then_some(check)
    }

    fn start_install(&mut self) {
        let payload = self.payload.clone();
        let opts = InstallOptions {
            install_dir: PathBuf::from(self.target_path()),
            desktop_shortcut: self.desktop_shortcut,
            install_vc_redist: self.vc_redist_missing && self.install_vc_redist,
        };
        let cancel = self.cancel.clone();
        let first = if self.launcher_running() {
            "Closing the launcher…"
        } else {
            "Copying files…"
        };
        self.outcome.set(crate::exit::OK);
        self.page = Page::Progress;
        self.pacer = ui::Pacer::new(first);
        self.rx = Some(ui::spawn_engine(move |sink| {
            perform_install(&payload, &opts, &cancel, sink)
        }));
    }

    fn reset_run(&mut self) {
        self.error = None;
        self.committed = false;
        self.cancelling = false;
        self.cancel = Cancel::default();
        self.warnings.clear();
        self.target_check = None;
        self.target_job = None;
        self.target_stale_since = None;
    }

    fn drain_events(&mut self) {
        while let Some(event) = ui::next_event(&mut self.rx, &mut self.pacer) {
            match event {
                EngineEvent::Warning(w) => self.warnings.push(w),
                EngineEvent::Committed => {
                    self.committed = true;
                    self.cancelling = false;
                }
                EngineEvent::Finished => {
                    self.webview2_missing = !crate::prereqs::webview2::is_installed();
                    self.pacer.finish();
                }
                EngineEvent::Cancelled => {
                    self.outcome.set(crate::exit::CANCELLED);
                    self.reset_run();
                    self.page = Page::Welcome;
                }
                EngineEvent::Failed(e) => {
                    self.outcome.set(crate::exit::FAILED);
                    self.error = Some(e);
                    self.page = Page::Done;
                }
                _ => {}
            }
        }
    }

    fn retry(&mut self) {
        self.reset_run();
        let path = self.target_path();
        let check = compute_target_check(
            &path,
            self.payload.manifest.estimated_size_kb,
            self.registered.as_deref(),
        );
        let allowed = !check.blocks_install();
        self.target_check = Some((path, std::time::Instant::now(), check));
        if allowed {
            self.start_install();
        } else {
            self.page = Page::Options;
        }
    }
}

impl eframe::App for WizardApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        ui::clear_color()
    }

    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.chrome.apply();
        self.drain_events();
        if matches!(self.page, Page::Welcome | Page::Options) {
            self.poll_target_check(ctx);
        }
        if self.rx.is_some() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        match self.page {
            Page::Progress => {
                self.pacer.tick();
                if self.pacer.settled() {
                    self.page = Page::Done;
                }
                ctx.request_repaint_after(std::time::Duration::from_millis(80));
            }
            Page::Welcome | Page::Options => {
                ctx.request_repaint_after(std::time::Duration::from_secs(1));
            }
            Page::Done => {
                if self.error.is_none() && !self.webview2_missing {
                    self.start_launcher_once();
                }
            }
        }

        let screen = self.screen();
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            match screen {
                Screen::Options => self.page = Page::Welcome,
                Screen::Progress => {}
                _ => ui::close(ctx),
            }
        }

        let closable = screen != Screen::Progress;
        let logo = self.logo.clone();
        let identity = format!("Version {}", self.payload.manifest.version);
        let rail = ui::Rail {
            logo: logo.as_ref(),
            identity,
            ..Default::default()
        };
        let rail = match screen {
            Screen::Welcome | Screen::Options => ui::Rail { step: Some(ui::Step::Setup), ..rail },
            Screen::Progress => ui::Rail {
                step: Some(ui::Step::Install),
                progress: ui::bar_fraction(self.pacer.percent()),
                ..rail
            },
            Screen::Done => ui::Rail {
                step: Some(ui::Step::Ready),
                badge: ui::Badge::Done,
                ..rail
            },
            Screen::Warnings => ui::Rail { step: Some(ui::Step::Ready), ..rail },
            Screen::Failed => ui::Rail {
                error: true,
                muted: true,
                badge: ui::Badge::Failed,
                ..rail
            },
        };
        let mood = match screen {
            Screen::Welcome | Screen::Options | Screen::Warnings => ui::Mood::CALM,
            Screen::Progress => ui::Mood::INSTALLING,
            Screen::Done => ui::Mood::DONE,
            Screen::Failed => ui::Mood::FAILED,
        };

        ui::split(root, mood, closable, rail, |body| match screen {
            Screen::Welcome => self.page_welcome(body),
            Screen::Options => self.page_options(body, frame),
            Screen::Progress => self.page_progress(body),
            Screen::Done => self.page_done(body, ctx),
            Screen::Warnings => self.page_warnings(body, ctx),
            Screen::Failed => self.page_failed(body, ctx),
        });
    }
}

impl WizardApp {
    fn page_welcome(&mut self, ui_: &mut egui::Ui) {
        ui::page_title(ui_, 16.0, "Setup", 29.0);

        let (replacing, elsewhere, blocker, size) = match self.shown_check() {
            Some(check) => (
                check.replacing,
                check.elsewhere.as_deref().map(elsewhere_notice),
                check
                    .refusal
                    .map(str::to_string)
                    .or_else(|| check.short_of_space()),
                check.size_line(),
            ),
            None => (false, None, None, Some(CHECKING_FOLDER.to_string())),
        };
        let mut notices: Vec<String> = Vec::new();
        if self.vc_redist_missing && self.install_vc_redist {
            notices.push(
                "The Microsoft Visual C++ runtime the games need is missing or out of date. \
                 Setup will install it, and Windows will ask for permission."
                    .to_string(),
            );
        }
        if replacing {
            notices.push(REPLACING_NOTICE.to_string());
        }
        notices.extend(elsewhere);

        ui_.scope(|g| {
            g.add_space(14.0);
            ui::paragraph(
                g,
                "Let's begin the setup and make the official launchers irrelevant, \
                 feel free to fill out your preferences along the way.",
                ui::text::BODY,
                ui::tx(0.68),
                340.0,
            );
            for (i, notice) in notices.iter().enumerate() {
                g.add_space(if i == 0 { 16.0 } else { 10.0 });
                ui::bullet_row(g, ui::NOTICE, notice);
            }
        });

        let ok = self.install_ready();
        let path = self.target_path();
        let mut start = false;
        let mut customize = false;
        ui::footer(ui_, |foot| {
            foot.scope(|foot| {
                ui::action_row(foot, true, |row| {
                    start = row
                        .add_enabled_ui(ok, |row| ui::primary_button(row, "Install"))
                        .inner
                        .clicked();
                    row.add_space(10.0);
                    customize = ui::secondary_button(row, "Customize").clicked();
                });
            });
            foot.add_space(14.0);
            foot.scope(|foot| {
                if let Some(blocker) = blocker {
                    ui::caption(foot, blocker, ui::DANGER_TEXT);
                    foot.add_space(3.0);
                } else if let Some(size) = size {
                    ui::caption(foot, size, ui::tx(0.56));
                    foot.add_space(3.0);
                }
                path_line(foot, &path);
                foot.add_space(3.0);
                ui::meta_label(foot, "Goes to");
                foot.add_space(14.0);
                ui::hairline(foot);
            });
        });

        if customize {
            self.page = Page::Options;
        } else if (start || Self::enter_pressed(ui_)) && ok {
            self.start_install();
        }
    }

    fn page_options(&mut self, ui_: &mut egui::Ui, frame: &eframe::Frame) {
        ui::page_title(ui_, 8.0, "Preferences", 22.0);

        ui_.scope(|g| {
            g.add_space(18.0);
            g.label(
                RichText::new("Install folder")
                    .size(ui::text::SM)
                    .color(ui::tx(0.60)),
            );
            g.add_space(7.0);
            g.horizontal(|row| {
                row.spacing_mut().item_spacing.x = 0.0;
                let width = row.available_width() - 8.0 - 84.0;
                ui::text_field(row, &mut self.install_dir, egui::vec2(width, 36.0));
                row.add_space(8.0);
                if ui::small_button(row, "Browse…").clicked() {
                    let mut dialog = rfd::FileDialog::new()
                        .set_title("Choose install folder")
                        .set_parent(frame);
                    if let Some(start) = existing_ancestor(&clean_target(&self.install_dir)) {
                        dialog = dialog.set_directory(start);
                    }
                    if let Some(dir) = dialog.pick_folder() {
                        let dir =
                            if dir.file_name().map(|n| n == consts::PRODUCT_NAME) == Some(true) {
                                dir
                            } else {
                                dir.join(consts::PRODUCT_NAME)
                            };
                        self.install_dir = dir.display().to_string();
                    }
                }
            });
            g.add_space(7.0);
            self.target_feedback(g);
        });

        ui_.scope(|g| {
            g.add_space(22.0);
            ui::checkbox(g, &mut self.desktop_shortcut, "Desktop shortcut");
            g.add_space(14.0);
            ui::checkbox(g, &mut self.launch_after, "Open when finished");
            if self.vc_redist_missing {
                g.add_space(14.0);
                ui::checkbox(g, &mut self.install_vc_redist, "Visual C++ runtime");
                g.add_space(3.0);
                ui::sub_note(
                    g,
                    "Kuro Games, HoYoverse, and Gryphline all use this framework. We can \
                     install it for you if you like.",
                );
            }
        });

        let ok = self.install_ready();
        let mut back = false;
        let mut start = false;
        ui::footer(ui_, |foot| {
            foot.scope(|foot| {
                ui::action_row_split(
                    foot,
                    |left| back = ui::secondary_button(left, "Back").clicked(),
                    |right| {
                        start = right
                            .add_enabled_ui(ok, |row| ui::primary_button(row, "Install"))
                            .inner
                            .clicked();
                    },
                );
            });
        });

        if back {
            self.page = Page::Welcome;
        } else if start {
            self.start_install();
        }
    }

    fn target_feedback(&mut self, ui_: &mut egui::Ui) {
        let Some(check) = self.shown_check() else {
            ui::caption(ui_, CHECKING_FOLDER, ui::tx(0.56));
            return;
        };
        let (refusal, replacing, foreign, short, size, elsewhere) = (
            check.refusal,
            check.replacing,
            check.foreign,
            check.short_of_space(),
            check.size_line(),
            check.elsewhere.as_deref().map(elsewhere_notice),
        );

        if let Some(refusal) = refusal {
            ui::caption(ui_, refusal, ui::DANGER_TEXT);
            return;
        }

        match (short, size) {
            (Some(short), _) => ui::caption(ui_, short, ui::DANGER_TEXT),
            (None, Some(size)) => ui::caption(ui_, size, ui::tx(0.56)),
            (None, None) => {}
        }

        let notice = if replacing {
            Some(REPLACING_NOTICE)
        } else if foreign {
            Some(
                "This folder already has files in it. Setup will add to it, and \
                 uninstalling will only remove what it put there.",
            )
        } else {
            None
        };
        if notice.is_some() || elsewhere.is_some() {
            ui_.add_space(10.0);
            ui::card(ui_, ui::Tone::Notice, 10, |card| {
                if let Some(notice) = notice {
                    ui::bullet_row(card, ui::NOTICE, notice);
                }
                if let Some(elsewhere) = &elsewhere {
                    if notice.is_some() {
                        card.add_space(10.0);
                    }
                    ui::bullet_row(card, ui::NOTICE, elsewhere);
                }
            });
        }
    }

    fn page_progress(&mut self, ui_: &mut egui::Ui) {
        ui::page_title(ui_, 14.0, "Installing", 25.0);
        let percent = self.pacer.percent();
        let label = if self.cancelling {
            "Stopping…"
        } else {
            self.pacer.phase()
        }
        .to_string();
        ui::progress_block(ui_, "install-progress", &label, percent);

        let stoppable = !self.cancelling && !self.committed && self.rx.is_some();
        let mut stop = false;
        if self.rx.is_some() {
            ui::footer(ui_, |foot| {
                foot.scope(|foot| {
                    ui::action_row(foot, true, |row| {
                        let button = row
                            .add_enabled_ui(stoppable, |row| ui::secondary_button(row, "Cancel"))
                            .inner;
                        let button = if self.committed {
                            button.on_disabled_hover_text(
                                "Too late to cancel now, setup is finishing up.",
                            )
                        } else {
                            button
                        };
                        stop = button.clicked();
                    });
                });
            });
        }
        if stop {
            self.cancelling = true;
            self.cancel.cancel();
        }
    }

    fn page_done(&mut self, ui_: &mut egui::Ui, ctx: &egui::Context) {
        ui::page_title(ui_, 16.0, "All set", 32.0);
        let launched = self.launched_at.is_some();
        ui_.scope(|g| {
            g.add_space(12.0);
            ui::paragraph(
                g,
                if launched {
                    "Opening the launcher."
                } else {
                    "Ready when you are."
                },
                ui::text::BODY,
                ui::tx(0.68),
                330.0,
            );
        });

        let countdown = self.auto_close_remaining();
        if let Some(left) = countdown {
            if left.is_zero() {
                ui::close(ctx);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
        let can_launch = !launched && !self.webview2_missing;
        let label = match countdown {
            Some(left) => format!("Close ({})", (left.as_secs() + 1).min(AUTO_CLOSE.as_secs())),
            None if can_launch => "Launch".to_string(),
            None => "Close".to_string(),
        };

        let path = self.target_path();
        let mut primary = false;
        let mut open_folder = false;
        ui::footer(ui_, |foot| {
            foot.scope(|foot| {
                ui::action_row(foot, true, |row| {
                    primary = ui::primary_button(row, &label).clicked();
                    row.add_space(10.0);
                    open_folder = ui::secondary_button(row, "Open folder").clicked();
                });
            });
            foot.add_space(14.0);
            foot.scope(|foot| {
                path_line(foot, &path);
                foot.add_space(3.0);
                ui::meta_label(foot, "Installed at");
                foot.add_space(14.0);
                ui::hairline(foot);
            });
        });

        if open_folder {
            self.stay_open = true;
            crate::win::open_folder(&PathBuf::from(&path));
        } else if primary || Self::enter_pressed(ui_) {
            if can_launch {
                self.launch_after = true;
                self.start_launcher_once();
                if self.warnings.is_empty() {
                    ui::close(ctx);
                }
            } else {
                ui::close(ctx);
            }
        }
    }

    fn page_warnings(&mut self, ui_: &mut egui::Ui, ctx: &egui::Context) {
        ui::page_title(ui_, 12.0, "Installed, but with errors", 28.0);
        let warnings = self.warnings.clone();
        let webview2_missing = self.webview2_missing;
        ui_.scope(|g| {
            g.add_space(20.0);
            ui::warnings_card(g, &warnings, 200.0);
            if !webview2_missing {
                g.add_space(16.0);
                ui::paragraph(
                    g,
                    "Nothing here stops the launcher from running, so feel free to ignore and \
                     gamble away.",
                    ui::text::BASE,
                    ui::tx(0.62),
                    330.0,
                );
            }
        });

        let mut close = false;
        let mut open_folder = false;
        let mut open_log = false;
        ui::footer(ui_, |foot| {
            foot.scope(|foot| {
                ui::action_row_split(
                    foot,
                    |left| open_log = ui::link(left, "Open log").clicked(),
                    |right| {
                        close = ui::primary_button(right, "Finish").clicked();
                        right.add_space(10.0);
                        open_folder = ui::secondary_button(right, "Open folder").clicked();
                    },
                );
            });
        });

        if open_log {
            ui::open_log_folder();
        } else if open_folder {
            crate::win::open_folder(&PathBuf::from(self.target_path()));
        } else if close {
            ui::close(ctx);
        }
    }

    fn start_launcher_once(&mut self) {
        if !self.launch_after || self.launched_at.is_some() {
            return;
        }
        let dir = PathBuf::from(self.target_path());
        match launch_app(&dir, &self.payload.manifest.main_binary) {
            Ok(()) => self.launched_at = Some(std::time::Instant::now()),
            Err(e) => {
                self.launch_after = false;
                self.warnings
                    .push(format!("Peebify Launcher could not be started ({e})."));
            }
        }
    }

    fn auto_close_remaining(&self) -> Option<std::time::Duration> {
        if !self.warnings.is_empty() || self.stay_open {
            return None;
        }
        let started = self.launched_at?;
        Some(AUTO_CLOSE.saturating_sub(started.elapsed()))
    }

    fn page_failed(&mut self, ui_: &mut egui::Ui, ctx: &egui::Context) {
        let err = self.error.clone().unwrap_or_default();
        let note = err.contains(crate::install::ROLLBACK_FAILED).then_some(
            "Setup could not put everything back. Run setup again to repair the install.",
        );
        match ui::failure_page(ui_, &mut self.copied, &err, "Install failed", note, true) {
            Some(ui::Failure::Retry) => self.retry(),
            Some(ui::Failure::Close) => ui::close(ctx),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_install_dir_keeps_a_valid_registered_location() {
        let custom = PathBuf::from(r"D:\Apps\Peebify Launcher");
        assert_eq!(initial_install_dir(Some(custom.clone())), custom);
    }

    #[test]
    fn clean_target_strips_copy_as_path_quotes() {
        assert_eq!(
            clean_target(r#"  "D:\Games\Peebify Launcher"  "#),
            r"D:\Games\Peebify Launcher"
        );
        assert_eq!(clean_target(r"D:\Games\Peebify Launcher"), r"D:\Games\Peebify Launcher");
    }

    fn ok_check() -> TargetCheck {
        TargetCheck {
            refusal: None,
            free: Some(10 << 30),
            needed: 1 << 20,
            replacing: false,
            foreign: false,
            elsewhere: None,
        }
    }

    #[test]
    fn install_needs_a_verdict_for_the_current_path() {
        let now = std::time::Instant::now();
        let seen = (r"D:\Games\Peebify Launcher".to_string(), now, ok_check());
        assert!(install_allowed(Some(&seen), r"D:\Games\Peebify Launcher"));
        assert!(!install_allowed(Some(&seen), r"\\nas\share\Peebify Launcher"));
        assert!(!install_allowed(None, r"D:\Games\Peebify Launcher"));
        let refused = (
            seen.0.clone(),
            now,
            TargetCheck {
                refusal: Some("no"),
                ..ok_check()
            },
        );
        assert!(!install_allowed(Some(&refused), &seen.0));
    }

    #[test]
    fn empty_target_is_refused_without_probing() {
        let check = compute_target_check("", 1024, None);
        assert_eq!(check.refusal, Some("Enter a folder to install into."));
        assert!(check.free.is_none() && !check.replacing && !check.foreign);
    }

    #[test]
    fn folder_picker_starts_at_the_deepest_existing_folder() {
        let temp = std::env::temp_dir();
        let typed = temp.join("peebify-no-such-dir").join("Peebify Launcher");
        assert_eq!(existing_ancestor(&typed.display().to_string()), Some(temp));
        assert_eq!(existing_ancestor(""), None);
    }

    #[test]
    fn initial_install_dir_skips_a_refused_registered_location() {
        let refused = PathBuf::from(r"C:\");
        assert_eq!(
            initial_install_dir(Some(refused)),
            consts::default_install_dir()
        );
    }
}
