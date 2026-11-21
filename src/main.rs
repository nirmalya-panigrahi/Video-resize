// Hide the console window on Windows release builds.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// (label, target height)
const PRESETS: [(&str, u32); 6] = [
    ("2160p (4K)", 2160),
    ("1440p (2K)", 1440),
    ("1080p (Full HD)", 1080),
    ("720p (HD)", 720),
    ("480p (SD)", 480),
    ("360p", 360),
];

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Preset,
    Custom,
    Percent,
}

enum Msg {
    Progress(f32),
    Done(Result<(), String>),
}

#[derive(Clone, Copy)]
struct SourceInfo {
    width: u32,
    height: u32,
    duration: f64,
}

struct Job {
    input: PathBuf,
    output: PathBuf,
    filter: String,
    crf: u8,
    target_mb: Option<f32>,
}

struct App {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    info: Option<SourceInfo>,
    mode: Mode,
    preset_idx: usize,
    width: u32,
    height: u32,
    keep_aspect: bool,
    percent: f32,
    limit_size: bool,
    target_mb: f32,
    crf: u8,
    rx: Option<Receiver<Msg>>,
    progress: f32,
    status: String,
    busy: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            info: None,
            mode: Mode::Preset,
            preset_idx: 3,
            width: 1280,
            height: 720,
            keep_aspect: true,
            percent: 50.0,
            limit_size: false,
            target_mb: 25.0,
            crf: 23,
            rx: None,
            progress: 0.0,
            status: "Drop a video here or click “Browse…”".into(),
            busy: false,
        }
    }
}

// ---------- helpers ----------

fn cmd(name: &str) -> Command {
    let mut c = Command::new(name);
    #[cfg(target_os = "windows")]
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

fn tool_available(name: &str) -> bool {
    cmd(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn probe(path: &PathBuf) -> Option<SourceInfo> {
    let out = cmd("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height:format=duration",
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut w, mut h, mut d) = (None, None, None);
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k {
                "width" => w = v.trim().parse().ok(),
                "height" => h = v.trim().parse().ok(),
                "duration" => d = v.trim().parse().ok(),
                _ => {}
            }
        }
    }
    Some(SourceInfo {
        width: w?,
        height: h?,
        duration: d.unwrap_or(0.0),
    })
}

fn even(x: f64) -> u32 {
    (((x as u32) / 2) * 2).max(2)
}

fn fmt_duration(secs: f64) -> String {
    let s = secs as u64;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

fn run_job(job: Job, tx: Sender<Msg>, ctx: egui::Context) {
    let send = |m: Msg| {
        let _ = tx.send(m);
        ctx.request_repaint();
    };

    let duration = probe(&job.input).map(|i| i.duration).unwrap_or(0.0);

    let mut c = cmd("ffmpeg");
    c.args([
        "-y",
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-progress",
        "pipe:1",
        "-i",
    ])
    .arg(&job.input)
    .args(["-vf", &job.filter, "-c:v", "libx264", "-preset", "medium"]);

    match job.target_mb {
        Some(mb) if duration > 0.0 => {
            // Approximate target: total kilobits / duration, minus audio, 5% safety margin.
            let total_kbps = mb as f64 * 8.0 * 1024.0 / duration;
            let video_kbps = ((total_kbps - 128.0) * 0.95).max(100.0);
            c.args([
                "-b:v",
                &format!("{video_kbps:.0}k"),
                "-maxrate",
                &format!("{:.0}k", video_kbps * 1.5),
                "-bufsize",
                &format!("{:.0}k", video_kbps * 2.0),
            ]);
        }
        _ => {
            c.args(["-crf", &job.crf.to_string()]);
        }
    }

    c.args(["-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart"])
        .arg(&job.output)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return send(Msg::Done(Err(format!("Could not start ffmpeg: {e}")))),
    };

    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("out_time_us=") {
                if let (Ok(us), true) = (v.trim().parse::<f64>(), duration > 0.0) {
                    let p = (us / 1_000_000.0 / duration).clamp(0.0, 1.0) as f32;
                    send(Msg::Progress(p));
                }
            }
        }
    }

    match child.wait_with_output() {
        Ok(out) if out.status.success() => send(Msg::Done(Ok(()))),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            send(Msg::Done(Err(if err.is_empty() {
                format!("ffmpeg exited with {}", out.status)
            } else {
                err.to_string()
            })))
        }
        Err(e) => send(Msg::Done(Err(e.to_string()))),
    }
}

// ---------- app logic ----------

impl App {
    fn set_input(&mut self, path: PathBuf) {
        self.info = probe(&path);
        if let Some(i) = self.info {
            self.width = i.width;
            self.height = i.height;
            self.status = "Ready.".into();
        } else {
            self.status = "Could not read that file as a video.".into();
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
        self.output = Some(path.with_file_name(format!("{stem}_resized.mp4")));
        self.input = Some(path);
        self.progress = 0.0;
    }

    fn filter(&self) -> String {
        match self.mode {
            Mode::Preset => format!("scale=-2:{}", PRESETS[self.preset_idx].1),
            Mode::Percent => {
                let f = self.percent as f64 / 100.0;
                format!("scale=trunc(iw*{f:.4}/2)*2:trunc(ih*{f:.4}/2)*2")
            }
            Mode::Custom => {
                let (w, h) = (self.width & !1, self.height & !1);
                if self.keep_aspect {
                    format!(
                        "scale={w}:{h}:force_original_aspect_ratio=decrease,\
                         scale=trunc(iw/2)*2:trunc(ih/2)*2"
                    )
                } else {
                    format!("scale={w}:{h}")
                }
            }
        }
    }

    fn output_dims(&self) -> Option<(u32, u32)> {
        let i = self.info?;
        let (sw, sh) = (i.width as f64, i.height as f64);
        Some(match self.mode {
            Mode::Preset => {
                let h = PRESETS[self.preset_idx].1 as f64;
                (even(sw * h / sh), even(h))
            }
            Mode::Percent => {
                let f = self.percent as f64 / 100.0;
                (even(sw * f), even(sh * f))
            }
            Mode::Custom if self.keep_aspect => {
                let s = (self.width as f64 / sw).min(self.height as f64 / sh);
                (even(sw * s), even(sh * s))
            }
            Mode::Custom => (even(self.width as f64), even(self.height as f64)),
        })
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (Some(input), Some(output)) = (self.input.clone(), self.output.clone()) else {
            return;
        };
        if input == output {
            self.status = "Output file must be different from the input file.".into();
            return;
        }
        let job = Job {
            input,
            output,
            filter: self.filter(),
            crf: self.crf,
            target_mb: self.limit_size.then_some(self.target_mb),
        };
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.busy = true;
        self.progress = 0.0;
        self.status = "Processing…".into();
        let ctx = ctx.clone();
        thread::spawn(move || run_job(job, tx, ctx));
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drag & drop
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if !self.busy {
            if let Some(p) = dropped.into_iter().find_map(|f| f.path) {
                self.set_input(p);
            }
        }

        // Messages from the worker thread
        let msgs: Vec<Msg> = self
            .rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for m in msgs {
            match m {
                Msg::Progress(p) => self.progress = p,
                Msg::Done(Ok(())) => {
                    self.busy = false;
                    self.progress = 1.0;
                    let out = self.output.as_ref().map(|p| p.display().to_string());
                    self.status = format!("Done! Saved to {}", out.unwrap_or_default());
                }
                Msg::Done(Err(e)) => {
                    self.busy = false;
                    self.status = format!("Error: {e}");
                }
            }
        }

        let mut start_clicked = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Video Resizer");
            ui.add_space(8.0);

            ui.add_enabled_ui(!self.busy, |ui| {
                // --- Input ---
                ui.horizontal(|ui| {
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("Video", &["mp4", "mov", "mkv", "avi", "webm", "m4v"])
                            .pick_file()
                        {
                            self.set_input(p);
                        }
                    }
                    match &self.input {
                        Some(p) => ui.label(p.display().to_string()),
                        None => ui.label("No video selected (you can drag & drop one)"),
                    };
                });
                if let Some(i) = self.info {
                    ui.label(format!(
                        "Source: {}×{}, {}",
                        i.width,
                        i.height,
                        fmt_duration(i.duration)
                    ));
                }

                ui.separator();

                // --- Size options ---
                ui.label("Final dimensions");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.mode, Mode::Preset, "Preset");
                    ui.radio_value(&mut self.mode, Mode::Custom, "Custom");
                    ui.radio_value(&mut self.mode, Mode::Percent, "Percent");
                });

                match self.mode {
                    Mode::Preset => {
                        egui::ComboBox::from_label("Resolution")
                            .selected_text(PRESETS[self.preset_idx].0)
                            .show_ui(ui, |ui| {
                                for (i, (name, _)) in PRESETS.iter().enumerate() {
                                    ui.selectable_value(&mut self.preset_idx, i, *name);
                                }
                            });
                    }
                    Mode::Custom => {
                        ui.horizontal(|ui| {
                            ui.label("Width");
                            ui.add(
                                egui::DragValue::new(&mut self.width)
                                    .range(2..=7680)
                                    .suffix(" px"),
                            );
                            ui.label("Height");
                            ui.add(
                                egui::DragValue::new(&mut self.height)
                                    .range(2..=4320)
                                    .suffix(" px"),
                            );
                        });
                        ui.checkbox(&mut self.keep_aspect, "Keep aspect ratio (fit inside box)");
                    }
                    Mode::Percent => {
                        ui.add(egui::Slider::new(&mut self.percent, 10.0..=100.0).suffix("%"));
                    }
                }

                if let Some((w, h)) = self.output_dims() {
                    ui.label(format!("Output size: {w}×{h}"));
                }

                ui.separator();

                // --- File size / quality ---
                ui.checkbox(&mut self.limit_size, "Target a file size");
                if self.limit_size {
                    ui.horizontal(|ui| {
                        ui.label("Approx. max size");
                        ui.add(
                            egui::DragValue::new(&mut self.target_mb)
                                .range(1.0..=20000.0)
                                .suffix(" MB"),
                        );
                    });
                    ui.weak("Approximate: the result may land slightly above or below.");
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Quality (CRF, lower = better)");
                        ui.add(egui::Slider::new(&mut self.crf, 16..=34));
                    });
                }

                ui.separator();

                // --- Output ---
                ui.horizontal(|ui| {
                    if ui.button("Save as…").clicked() {
                        let mut dlg = rfd::FileDialog::new().add_filter("MP4 video", &["mp4"]);
                        if let Some(name) = self
                            .output
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .and_then(|n| n.to_str())
                        {
                            dlg = dlg.set_file_name(name);
                        }
                        if let Some(p) = dlg.save_file() {
                            self.output = Some(p);
                        }
                    }
                    match &self.output {
                        Some(p) => ui.label(p.display().to_string()),
                        None => ui.label("—"),
                    };
                });

                ui.add_space(8.0);
                let ready = self.input.is_some() && self.output.is_some();
                if ui
                    .add_enabled(ready, egui::Button::new("▶  Process video"))
                    .clicked()
                {
                    start_clicked = true;
                }
            });

            ui.add_space(8.0);
            if self.busy || self.progress > 0.0 {
                ui.add(egui::ProgressBar::new(self.progress).show_percentage());
            }
            ui.label(&self.status);
        });

        if start_clicked {
            if !tool_available("ffmpeg") {
                self.status = "ffmpeg was not found. Install it and add it to your PATH.".into();
            } else {
                self.start(ctx);
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Video Resizer",
        options,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
}
