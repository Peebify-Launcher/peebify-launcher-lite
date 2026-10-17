// ------------ Uninstall Window ------------
// What opens from Installed apps or uninstall.exe, running from the temp copy setup makes of itself. Lets you also
// delete settings and installed games (Steam and protected folders are kept), then removes everything in the same
// window and ends on All done, a partial result or Failed with Retry.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};

use crate::install::{
    dir_size, game_dir_verdict, human_bytes, installed_game_dirs, perform_uninstall,
    respawn_for_uninstall, GameDirVerdict, InstallManifest, SecondStage, UninstallOptions,
};
use crate::msg::EngineEvent;
use crate::ui;

pub enum Start {
    Choose,
    Remove(UninstallOptions),
}

pub struct Launch {
    pub install_dir: PathBuf,
    pub staged: bool,
    pub start: Start,
}

fn identity_line(install_dir: &Path) -> String {
    match InstallManifest::load(install_dir) {
        Ok(manifest) => format!("Version {}", manifest.version),
        Err(_) => "Installed on this PC".to_string(),
    }
}

struct GameDir {
    path: PathBuf,
    verdict: GameDirVerdict,
}

impl GameDir {
    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

fn measure_games(dirs: Vec<PathBuf>, sizes: Arc<Mutex<Vec<Option<u64>>>>, ctx: egui::Context) {
    std::thread::spawn(move || {
        for (i, dir) in dirs.iter().enumerate() {
            let size = dir_size(dir);
            if let Ok(mut sizes) = sizes.lock() {
                sizes[i] = Some(size);
            }
            ctx.request_repaint();
        }
    });
}

pub fn run(launch: Launch) -> eframe::Result<i32> {
    let identity = identity_line(&launch.install_dir);
    let games: Vec<GameDir> = match launch.start {
        Start::Choose => installed_game_dirs(&launch.install_dir)
            .into_iter()
            .map(|path| {
                let verdict = game_dir_verdict(&path);
                GameDir { path, verdict }
            })
            .collect(),
        Start::Remove(_) => Vec::new(),
    };
    ui::run_window("Uninstall Peebify Launcher", ui::WINDOW, move |ctx, outcome| {
        let sizes = Arc::new(Mutex::new(vec![None; games.len()]));
        measure_games(
            games.iter().map(|g| g.path.clone()).collect(),
            sizes.clone(),
            ctx.clone(),
        );
        let mut app = UninstallApp {
            logo: ui::load_logo(ctx),
            identity,
            install_dir: launch.install_dir,
            staged: launch.staged,
            games,
            sizes,
            remove_data: false,
            remove_games: false,
            screen: Screen::Choose,
            rx: None,
            pacer: ui::Pacer::new(""),
            warnings: Vec::new(),
            error: None,
            respawn_error: None,
            last_run: None,
            copied: ui::Copied::default(),
            chrome: ui::Chrome::default(),
            outcome,
        };
        if let Start::Remove(opts) = launch.start {
            app.start_removal(opts);
        }
        Box::new(app)
    })
}

#[derive(Clone, Copy, PartialEq, Hash, Debug)]
enum Screen {
    Choose,
    Removing,
    Removed,
    Failed,
}

struct UninstallApp {
    logo: Option<egui::TextureHandle>,
    identity: String,
    install_dir: PathBuf,
    staged: bool,
    games: Vec<GameDir>,
    sizes: Arc<Mutex<Vec<Option<u64>>>>,
    remove_data: bool,
    remove_games: bool,
    screen: Screen,
    rx: Option<std::sync::mpsc::Receiver<EngineEvent>>,
    pacer: ui::Pacer,
    warnings: Vec<String>,
    error: Option<String>,
    respawn_error: Option<String>,
    last_run: Option<UninstallOptions>,
    copied: ui::Copied,
    chrome: ui::Chrome,
    outcome: std::rc::Rc<std::cell::Cell<i32>>,
}

impl UninstallApp {
    fn any_removable(&self) -> bool {
        self.games
            .iter()
            .any(|g| g.verdict == GameDirVerdict::Removable)
    }

    fn options(&self) -> UninstallOptions {
        UninstallOptions {
            install_dir: self.install_dir.clone(),
            remove_data: self.remove_data,
            remove_games: self.remove_games && self.any_removable(),
        }
    }

    fn start_removal(&mut self, opts: UninstallOptions) {
        self.error = None;
        self.warnings.clear();
        self.outcome.set(crate::exit::OK);
        self.pacer = ui::Pacer::new("Closing the launcher…");
        self.screen = Screen::Removing;
        self.last_run = Some(opts.clone());
        self.rx = Some(ui::spawn_engine(move |sink| perform_uninstall(&opts, sink)));
    }

    fn drain_events(&mut self) {
        while let Some(event) = ui::next_event(&mut self.rx, &mut self.pacer) {
            match event {
                EngineEvent::Warning(w) => self.warnings.push(w),
                EngineEvent::Finished => self.pacer.finish_as("Cleaning up…"),
                EngineEvent::Cancelled => {
                    self.outcome.set(crate::exit::CANCELLED);
                    self.screen = Screen::Choose;
                }
                EngineEvent::Failed(e) => {
                    self.outcome.set(crate::exit::FAILED);
                    self.error = Some(e);
                    self.screen = Screen::Failed;
                }
                _ => {}
            }
        }
    }

    fn begin(&mut self, ctx: &egui::Context) {
        let opts = self.options();
        if self.staged {
            self.start_removal(opts);
            return;
        }
        match respawn_for_uninstall(&opts, SecondStage::Run) {
            Ok(()) => ui::close(ctx),
            Err(e) => self.respawn_error = Some(e),
        }
    }
}

impl eframe::App for UninstallApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        ui::clear_color()
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.chrome.apply();
        self.drain_events();
        if self.rx.is_some() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.screen == Screen::Removing {
            self.pacer.tick();
            if self.rx.is_none() && self.pacer.settled() {
                self.screen = Screen::Removed;
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && self.screen != Screen::Removing {
            ui::close(ctx);
        }

        let screen = self.screen;
        let logo = self.logo.clone();
        let rail = ui::Rail {
            logo: logo.as_ref(),
            identity: self.identity.clone(),
            ..Default::default()
        };
        let rail = if screen == Screen::Failed {
            ui::Rail { error: true, muted: true, badge: ui::Badge::Failed, ..rail }
        } else {
            ui::Rail { muted: true, ..rail }
        };
        let mood = match screen {
            Screen::Choose => ui::Mood::UNINSTALL,
            Screen::Removing => ui::Mood::REMOVING,
            Screen::Removed => ui::Mood::REMOVED,
            Screen::Failed => ui::Mood::FAILED,
        };
        let closable = screen != Screen::Removing;

        ui::split(root, mood, closable, rail, |body| match screen {
            Screen::Choose => self.page_choose(body, ctx),
            Screen::Removing => self.page_removing(body),
            Screen::Removed => self.page_removed(body, ctx),
            Screen::Failed => self.page_failed(body, ctx),
        });
    }
}

impl UninstallApp {
    fn page_choose(&mut self, ui_: &mut egui::Ui, ctx: &egui::Context) {
        ui::page_title(ui_, 12.0, "Uninstall", 26.0);

        let removable = self.any_removable();
        ui_.scope(|g| {
            g.add_space(20.0);
            ui::card(g, ui::Tone::Plain, 15, |card| {
                ui::checkbox(card, &mut self.remove_data, "Delete settings & mods");
                if removable {
                    card.add_space(13.0);
                    ui::checkbox(card, &mut self.remove_games, "Delete game files");
                    self.game_list(card);
                }
            });
            if let Some(err) = &self.respawn_error {
                g.add_space(10.0);
                ui::paragraph(g, err, ui::text::XS, ui::DANGER_TEXT, 400.0);
            }
        });

        let mut remove = false;
        let mut keep = false;
        ui::footer(ui_, |foot| {
            foot.scope(|foot| {
                ui::action_row(foot, true, |row| {
                    remove = ui::danger_button(row, "Uninstall").clicked();
                    row.add_space(10.0);
                    keep = ui::secondary_button(row, "Keep it").clicked();
                });
            });
        });

        if keep {
            ui::close(ctx);
        } else if remove {
            self.begin(ctx);
        }
    }

    fn game_list(&mut self, ui_: &mut egui::Ui) {
        let ctx = ui_.ctx().clone();
        let id = egui::Id::new("uninstall-games");
        let open = ui::motion::tween(
            &ctx,
            id.with("open"),
            ui::motion::flag(self.remove_games),
            ui::motion::Spec::new(460, ui::motion::Curve::Out),
        );
        let fade = ui::motion::tween(
            &ctx,
            id.with("fade"),
            ui::motion::flag(self.remove_games),
            ui::motion::Spec::new(300, ui::motion::Curve::Ease),
        );
        if open <= 0.001 {
            return;
        }

        let full = ctx.data(|d| d.get_temp::<f32>(id.with("height"))).unwrap_or(0.0);
        let shown = (full * open).max(0.0);
        let width = ui_.available_width();
        let top = ui_.cursor().min;
        let mut list = ui_.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_size(top, egui::vec2(width, f32::INFINITY)))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        list.set_clip_rect(
            egui::Rect::from_min_size(top, egui::vec2(width, shown)).intersect(ui_.clip_rect()),
        );
        list.multiply_opacity(fade.clamp(0.0, 1.0));
        list.spacing_mut().item_spacing.y = 0.0;
        list.add_space(11.0);
        let sizes = self.sizes.lock().map(|s| s.clone()).unwrap_or_default();
        egui::ScrollArea::vertical()
            .max_height(132.0)
            .auto_shrink([false, true])
            .show(&mut list, |rows| {
                rows.spacing_mut().item_spacing.y = 6.0;
                for (i, game) in self.games.iter().enumerate() {
                    let kept = match game.verdict {
                        GameDirVerdict::Removable => None,
                        GameDirVerdict::Steam => Some("Kept by Steam"),
                        GameDirVerdict::Guarded => Some("Kept"),
                    };
                    let (name_color, detail, detail_color) = match kept {
                        Some(tag) => (ui::tx(0.45), tag.to_string(), ui::tx(0.45)),
                        None => (
                            ui::tx(0.75),
                            sizes
                                .get(i)
                                .copied()
                                .flatten()
                                .map(human_bytes)
                                .unwrap_or_else(|| "…".to_string()),
                            ui::tx(0.56),
                        ),
                    };
                    rows.horizontal(|row| {
                        row.add_space(28.0);
                        row.with_layout(egui::Layout::right_to_left(egui::Align::Center), |row| {
                            row.label(
                                RichText::new(detail)
                                    .size(ui::text::SM)
                                    .color(detail_color),
                            );
                            row.add_space(12.0);
                            row.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |row| {
                                    row.add(
                                        egui::Label::new(
                                            RichText::new(game.name())
                                                .size(ui::text::SM)
                                                .color(name_color),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(game.path.display().to_string());
                                },
                            );
                        });
                    });
                }
            });
        let measured = list.min_rect().height();
        ctx.data_mut(|d| d.insert_temp(id.with("height"), measured));
        if (measured - full).abs() > 0.5 {
            ctx.request_repaint();
        }
        ui_.allocate_exact_size(egui::vec2(width, shown), egui::Sense::hover());
    }

    fn page_removing(&mut self, ui_: &mut egui::Ui) {
        ui::page_title(ui_, 14.0, "Uninstalling", 25.0);
        let percent = self.pacer.percent();
        let label = self.pacer.phase().to_string();
        ui::progress_block(ui_, "uninstall-progress", &label, percent);
    }

    fn page_removed(&mut self, ui_: &mut egui::Ui, ctx: &egui::Context) {
        let partial = !self.warnings.is_empty();
        ui::page_title(ui_, 16.0, if partial { "Mostly done" } else { "All done" }, 32.0);
        let warnings = self.warnings.clone();
        ui_.scope(|g| {
            g.add_space(12.0);
            ui::paragraph(
                g,
                if partial {
                    "Some items could not be removed."
                } else {
                    "Hope to see you again."
                },
                ui::text::BODY,
                ui::tx(0.68),
                330.0,
            );
            if partial {
                g.add_space(16.0);
                ui::warnings_card(g, &warnings, 180.0);
            }
        });

        let mut close = false;
        let mut open_log = false;
        ui::footer(ui_, |foot| {
            foot.scope(|foot| {
                ui::action_row_split(
                    foot,
                    |left| {
                        if partial {
                            open_log = ui::link(left, "Open log").clicked();
                        }
                    },
                    |right| close = ui::primary_button(right, "Close").clicked(),
                );
            });
        });
        if open_log {
            ui::open_log_folder();
        } else if close {
            ui::close(ctx);
        }
    }

    fn page_failed(&mut self, ui_: &mut egui::Ui, ctx: &egui::Context) {
        let err = self.error.clone().unwrap_or_default();
        let can_retry = self.last_run.is_some();
        match ui::failure_page(
            ui_,
            &mut self.copied,
            &err,
            "Uninstall failed",
            None,
            can_retry,
        ) {
            Some(ui::Failure::Retry) => {
                if let Some(opts) = self.last_run.clone() {
                    self.start_removal(opts);
                }
            }
            Some(ui::Failure::Close) => ui::close(ctx),
            None => {}
        }
    }
}
