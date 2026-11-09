#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod encode;

use eframe::egui;
use encode::{Failure, Mode, Settings, Tools, presets};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::{fs, thread};

const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "avi", "webm", "m4v", "wmv", "flv", "3gp", "ts"];
const RESOLUTIONS: &[(u32, &str)] = &[(0, "Original"), (480, "480p"), (720, "720p"), (1080, "1080p")];
const SPEEDS: &[&str] = &["fast", "medium", "slow", "veryslow"];
const AUDIO_KBPS: &[u32] = &[64, 96, 128, 160, 192];
/// Width of the label column, shared so the settings and output sections line up.
const LABEL_WIDTH: f32 = 90.0;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 620.0])
            .with_min_inner_size([420.0, 460.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("TG Compress", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// Remembered between launches.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    settings: Settings,
    /// None = save next to the original.
    out_dir: Option<PathBuf>,
    suffix: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            settings: presets()[0].1.clone(),
            out_dir: None,
            suffix: "_tg".into(),
        }
    }
}

enum Status {
    Queued,
    Running { progress: f32, stage: String },
    Done { output: PathBuf, size: u64 },
    Failed(String),
    Cancelled,
}

struct Job {
    input: PathBuf,
    size: u64,
    status: Status,
    /// Latest estimated output size and the settings it was computed for.
    estimate: Option<(Settings, EstimateResult)>,
    /// Settings an estimate was last requested for, so each change asks only once.
    requested: Option<Settings>,
}

type EstimateResult = Result<u64, String>;

/// Background size estimates, cached per (file, settings) so revisiting a preset is instant.
struct Estimator {
    tx: mpsc::Sender<PathBuf>,
    rx: mpsc::Receiver<(PathBuf, Settings, EstimateResult)>,
    current: Arc<Mutex<Settings>>,
}

impl Estimator {
    fn new(tools: Tools, settings: Settings, ctx: egui::Context) -> Self {
        let (tx, requests) = mpsc::channel::<PathBuf>();
        let (results, rx) = mpsc::channel();
        let current = Arc::new(Mutex::new(settings));
        let latest = current.clone();
        thread::spawn(move || {
            let mut cache: HashMap<(PathBuf, Settings), EstimateResult> = HashMap::new();
            for input in requests {
                // Estimate for the settings as they are now; requests queued while a slider
                // was dragging all collapse onto the final value via the cache.
                let settings = latest.lock().unwrap().clone();
                let result = cache
                    .entry((input.clone(), settings.clone()))
                    .or_insert_with(|| encode::estimate(&tools, &input, &settings))
                    .clone();
                if results.send((input, settings, result)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self { tx, rx, current }
    }
}

enum Msg {
    Progress(usize, f32, String),
    Done(usize, PathBuf, u64),
    Failed(usize, String),
    Cancelled(usize),
    Finished,
}

struct Worker {
    rx: mpsc::Receiver<Msg>,
    cancel: Arc<AtomicBool>,
}

struct App {
    prefs: Prefs,
    tools: Option<Tools>,
    jobs: Vec<Job>,
    worker: Option<Worker>,
    estimator: Option<Estimator>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let prefs: Prefs = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        let tools = Tools::find();
        let estimator = tools
            .clone()
            .map(|t| Estimator::new(t, prefs.settings.clone(), cc.egui_ctx.clone()));
        Self { prefs, tools, jobs: Vec::new(), worker: None, estimator }
    }

    fn add_files(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for input in paths {
            if input.is_file() && !self.jobs.iter().any(|j| j.input == input) {
                let size = fs::metadata(&input).map_or(0, |m| m.len());
                self.jobs.push(Job { input, size, status: Status::Queued, estimate: None, requested: None });
            }
        }
    }

    fn update_estimates(&mut self) {
        let Some(estimator) = &self.estimator else { return };
        let settings = &self.prefs.settings;
        {
            let mut current = estimator.current.lock().unwrap();
            if *current != *settings {
                *current = settings.clone();
            }
        }
        while let Ok((input, used, result)) = estimator.rx.try_recv() {
            if let Some(job) = self.jobs.iter_mut().find(|j| j.input == input) {
                job.estimate = Some((used, result));
            }
        }
        for job in &mut self.jobs {
            if matches!(job.status, Status::Queued) && job.requested.as_ref() != Some(settings) {
                let _ = estimator.tx.send(job.input.clone());
                job.requested = Some(settings.clone());
            }
        }
    }

    fn pending(&self) -> usize {
        self.jobs.iter().filter(|j| !matches!(j.status, Status::Done { .. })).count()
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(tools) = self.tools.clone() else { return };
        let queue: Vec<(usize, PathBuf)> = self
            .jobs
            .iter_mut()
            .enumerate()
            .filter(|(_, j)| !matches!(j.status, Status::Done { .. }))
            .map(|(i, j)| {
                j.status = Status::Queued;
                (i, j.input.clone())
            })
            .collect();

        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let settings = self.prefs.settings.clone();
        let out_dir = self.prefs.out_dir.clone();
        let suffix = self.prefs.suffix.clone();
        let (ctx, stop) = (ctx.clone(), cancel.clone());

        thread::spawn(move || {
            for (i, input) in queue {
                if stop.load(Ordering::Relaxed) {
                    let _ = tx.send(Msg::Cancelled(i));
                    continue;
                }
                let dir = out_dir
                    .clone()
                    .or_else(|| input.parent().map(Path::to_path_buf))
                    .unwrap_or_default();
                let output = encode::unique_output(&dir, &input, &suffix);
                let result = encode::compress(&tools, &input, &output, &settings, &stop, |p, stage| {
                    let _ = tx.send(Msg::Progress(i, p, stage.to_string()));
                    ctx.request_repaint();
                });
                let msg = match result {
                    Ok(()) => {
                        let size = fs::metadata(&output).map_or(0, |m| m.len());
                        Msg::Done(i, output, size)
                    }
                    Err(failure) => {
                        let _ = fs::remove_file(&output);
                        match failure {
                            Failure::Cancelled => Msg::Cancelled(i),
                            Failure::Error(e) => Msg::Failed(i, e),
                        }
                    }
                };
                let _ = tx.send(msg);
                ctx.request_repaint();
            }
            let _ = tx.send(Msg::Finished);
            ctx.request_repaint();
        });

        self.worker = Some(Worker { rx, cancel });
    }

    fn poll_worker(&mut self) {
        let Some(worker) = &self.worker else { return };
        let mut finished = false;
        while let Ok(msg) = worker.rx.try_recv() {
            match msg {
                Msg::Progress(i, progress, stage) => self.jobs[i].status = Status::Running { progress, stage },
                Msg::Done(i, output, size) => self.jobs[i].status = Status::Done { output, size },
                Msg::Failed(i, e) => self.jobs[i].status = Status::Failed(e),
                Msg::Cancelled(i) => self.jobs[i].status = Status::Cancelled,
                Msg::Finished => finished = true,
            }
        }
        if finished {
            self.worker = None;
        }
    }

    fn drop_zone(&mut self, ui: &mut egui::Ui) {
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 80.0), egui::Sense::click());
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);

        let visuals = ui.visuals();
        let (fill, stroke) = if hovering || response.hovered() {
            (visuals.selection.bg_fill.gamma_multiply(0.25), visuals.selection.stroke.color)
        } else {
            (visuals.faint_bg_color, visuals.widgets.noninteractive.bg_stroke.color)
        };
        let painter = ui.painter();
        painter.rect(rect, 8.0, fill, egui::Stroke::new(1.5, stroke), egui::StrokeKind::Inside);
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop videos here or click to browse",
            egui::FontId::proportional(16.0),
            visuals.text_color(),
        );

        if response.clicked()
            && let Some(files) = rfd::FileDialog::new().add_filter("Video", VIDEO_EXTS).pick_files()
        {
            self.add_files(files);
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.prefs;
        egui::Grid::new("settings").num_columns(2).min_col_width(LABEL_WIDTH).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Preset");
            let all = presets();
            let current = all.iter().find(|(_, s)| *s == p.settings).map_or("Custom", |(n, _)| *n);
            egui::ComboBox::from_id_salt("preset").selected_text(current).width(240.0).show_ui(ui, |ui| {
                for (name, preset) in all {
                    if ui.selectable_label(current == name, name).clicked() {
                        p.settings = preset;
                    }
                }
            });
            ui.end_row();

            let s = &mut p.settings;
            ui.label("Mode");
            ui.horizontal(|ui| {
                ui.radio_value(&mut s.mode, Mode::Quality, "Quality");
                ui.radio_value(&mut s.mode, Mode::Size, "Fit size");
            });
            ui.end_row();

            match s.mode {
                Mode::Quality => {
                    ui.label("Quality (CRF)");
                    ui.add(egui::Slider::new(&mut s.crf, 18..=32))
                        .on_hover_text("Lower = better quality, bigger file");
                }
                Mode::Size => {
                    ui.label("Max size");
                    ui.add(egui::DragValue::new(&mut s.target_mb).range(1..=4000).suffix(" MB"));
                }
            }
            ui.end_row();

            ui.label("Resolution");
            combo(ui, "res", &mut s.max_res, RESOLUTIONS.iter().map(|&(v, l)| (v, l.to_string())));
            ui.end_row();

            ui.label("Speed").on_hover_text("Slower = smaller file at the same quality");
            combo(ui, "speed", &mut s.speed, SPEEDS.iter().map(|&v| (v.to_string(), v.to_string())));
            ui.end_row();

            ui.label("Audio");
            combo(ui, "audio", &mut s.audio_kbps, AUDIO_KBPS.iter().map(|&v| (v, format!("{v} kbps"))));
            ui.end_row();
        });
    }

    fn output_ui(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.prefs;
        egui::Grid::new("output").num_columns(2).min_col_width(LABEL_WIDTH).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Save to");
            ui.horizontal(|ui| {
                if ui.radio(p.out_dir.is_none(), "Next to original").clicked() {
                    p.out_dir = None;
                }
                if ui.radio(p.out_dir.is_some(), "Folder…").clicked() && p.out_dir.is_none() {
                    p.out_dir = rfd::FileDialog::new().pick_folder();
                }
            });
            ui.end_row();

            ui.label("File name");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut p.suffix).desired_width(60.0))
                    .on_hover_text("Added to the original name");
                ui.weak(format!("→ video{}.mp4", p.suffix));
            });
            ui.end_row();
        });

        // Outside the grid: grids grow to fit their content, so a long path would widen the window.
        if let Some(dir) = &p.out_dir {
            let mut change = false;
            ui.horizontal(|ui| {
                ui.add_space(LABEL_WIDTH + 12.0);
                change = ui.small_button("Change…").clicked();
                ui.add(egui::Label::new(tilde(dir)).truncate()).on_hover_text(dir.display().to_string());
            });
            if change && let Some(dir) = rfd::FileDialog::new().pick_folder() {
                p.out_dir = Some(dir);
            }
        }
    }

    /// Total input size and, once every file has a size for the current settings, the expected output.
    fn totals(&self) -> (u64, Option<u64>) {
        let input = self.jobs.iter().map(|j| j.size).sum();
        let output = self
            .jobs
            .iter()
            .map(|j| match (&j.status, &j.estimate) {
                (Status::Done { size, .. }, _) => Some(*size),
                (_, Some((used, Ok(bytes)))) if *used == self.prefs.settings => Some(*bytes),
                _ => None,
            })
            .sum();
        (input, output)
    }

    fn jobs_header_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong(format!("Videos ({})", self.jobs.len()));
            if !self.jobs.is_empty() {
                let (input, output) = self.totals();
                let output = output.map_or("…".into(), |b| format!("≈ {}", human_size(b)));
                ui.weak(format!("{} → {output}", human_size(input)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.worker.is_none() && !self.jobs.is_empty() && ui.small_button("Clear").clicked() {
                    self.jobs.clear();
                }
            });
        });
    }

    fn jobs_ui(&mut self, ui: &mut egui::Ui, height: f32) {
        let busy = self.worker.is_some();
        let mut remove = None;
        egui::ScrollArea::vertical().max_height(height).auto_shrink(false).show(ui, |ui| {
            if self.jobs.is_empty() {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| ui.weak("Added videos appear here."));
            }
            for (i, job) in self.jobs.iter().enumerate() {
                match job_row(ui, job, &self.prefs.settings, busy) {
                    RowAction::Remove => remove = Some(i),
                    RowAction::Show(path) => reveal(&path),
                    RowAction::None => {}
                }
                ui.add_space(4.0);
            }
        });
        if let Some(i) = remove {
            self.jobs.remove(i);
        }
    }

    fn actions_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(worker) = &self.worker {
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(100.0, 32.0))).clicked() {
                    worker.cancel.store(true, Ordering::Relaxed);
                }
                let left = self
                    .jobs
                    .iter()
                    .filter(|j| matches!(j.status, Status::Queued | Status::Running { .. }))
                    .count();
                ui.weak(format!("Compressing… {left} left"));
            });
            return;
        }
        let pending = self.pending();
        let label = match pending {
            0 => "Add videos to compress".to_string(),
            1 => "Compress 1 video".to_string(),
            n => format!("Compress {n} videos"),
        };
        let button = egui::Button::new(egui::RichText::new(label).strong())
            .min_size(egui::vec2(ui.available_width(), 32.0));
        if ui.add_enabled(pending > 0 && self.tools.is_some(), button).clicked() {
            self.start(ui.ctx());
        }
    }
}

enum RowAction {
    None,
    Remove,
    Show(PathBuf),
}

const ICON_WIDTH: f32 = 16.0;

/// One video: status icon and name, with sizes or progress on the line below.
fn job_row(ui: &mut egui::Ui, job: &Job, settings: &Settings, busy: bool) -> RowAction {
    let mut action = RowAction::None;
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                status_icon(ui, &job.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !busy && ui.small_button("🗙").on_hover_text("Remove from list").clicked() {
                        action = RowAction::Remove;
                    }
                    if let Status::Done { output, .. } = &job.status
                        && ui.small_button("Show").on_hover_text("Show in Finder").clicked()
                    {
                        action = RowAction::Show(output.clone());
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let name = job.input.file_name().unwrap_or_default().to_string_lossy();
                        ui.add(egui::Label::new(egui::RichText::new(name).strong()).truncate())
                            .on_hover_text(job.input.display().to_string());
                    });
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(ICON_WIDTH + ui.spacing().item_spacing.x);
                ui.vertical(|ui| job_detail(ui, job, settings));
            });
        });
    action
}

fn status_icon(ui: &mut egui::Ui, status: &Status) {
    let size = egui::vec2(ICON_WIDTH, ICON_WIDTH);
    let (icon, color) = match status {
        Status::Running { .. } => {
            ui.add_sized(size, egui::Spinner::new());
            return;
        }
        Status::Queued => ("•", ui.visuals().weak_text_color()),
        Status::Done { .. } => ("✔", egui::Color32::from_rgb(70, 175, 95)),
        Status::Failed(_) => ("✖", ui.visuals().error_fg_color),
        Status::Cancelled => ("–", ui.visuals().weak_text_color()),
    };
    ui.add_sized(size, egui::Label::new(egui::RichText::new(icon).color(color)));
}

fn job_detail(ui: &mut egui::Ui, job: &Job, settings: &Settings) {
    let original = human_size(job.size);
    match &job.status {
        Status::Queued => match &job.estimate {
            Some((used, Ok(bytes))) if used == settings => {
                // Size mode is a bitrate budget: simple footage can come in under it.
                let approx = if used.mode == Mode::Size { "up to" } else { "≈" };
                ui.weak(format!("{original} → {approx} {}", human_size(*bytes)))
                    .on_hover_text("Estimated output size");
            }
            Some((used, Err(e))) if used == settings => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            _ => {
                ui.weak(format!("{original} → estimating…"));
            }
        },
        Status::Running { progress, stage } => {
            ui.add(
                egui::ProgressBar::new(*progress)
                    .desired_height(16.0)
                    .text(format!("{stage} · {:.0}%", progress * 100.0)),
            );
        }
        Status::Done { size, .. } => {
            let saved = 100.0 - *size as f64 / job.size.max(1) as f64 * 100.0;
            ui.label(format!("{original} → {} · {saved:.0}% smaller", human_size(*size)));
        }
        Status::Failed(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        Status::Cancelled => {
            ui.weak("Cancelled");
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_worker();
        self.update_estimates();
        let dropped: Vec<PathBuf> =
            ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        self.add_files(dropped);

        egui::CentralPanel::default().show(ui, |ui| {
            if self.tools.is_none() {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    "ffmpeg not found. Install it with:  brew install ffmpeg",
                );
                ui.add_space(6.0);
            }
            self.drop_zone(ui);
            ui.add_space(14.0);
            self.settings_ui(ui);
            ui.add_space(14.0);
            self.output_ui(ui);
            ui.add_space(10.0);
            ui.separator();
            self.jobs_header_ui(ui);
            ui.add_space(4.0);
            let actions_height = 32.0 + 2.0 * ui.spacing().item_spacing.y;
            self.jobs_ui(ui, (ui.available_height() - actions_height).max(80.0));
            self.actions_ui(ui);
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.prefs);
    }
}

fn combo<T: PartialEq + Clone>(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut T,
    options: impl Iterator<Item = (T, String)>,
) {
    let options: Vec<_> = options.collect();
    let selected = options.iter().find(|(v, _)| v == value).map_or("—".into(), |(_, l)| l.clone());
    egui::ComboBox::from_id_salt(id).selected_text(selected).show_ui(ui, |ui| {
        for (v, label) in options {
            ui.selectable_value(value, v, label);
        }
    });
}

fn human_size(bytes: u64) -> String {
    let mb = bytes as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 { format!("{:.2} GB", mb / 1024.0) } else { format!("{mb:.1} MB") }
}

fn reveal(path: &Path) {
    let _ = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-R").arg(path).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn()
    };
}

/// Path for display, with the home folder shortened to `~`.
fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}
