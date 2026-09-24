use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use eframe::egui;
use vzip::encode::{FlagSink, Progress};
use vzip::plan::{plan, Settings};
use vzip::probe::{probe, MediaInfo};
use vzip::resolve::resolve;
use vzip::{compress, fmt_bytes, fmt_duration, CompressConfig, CompressResult, DEFAULT_AUDIO_KBPS};

const ALLOWED_EXT: [&str; 4] = ["mp4", "mov", "m4v", "avi"];
const QUICK_SIZES: [u64; 3] = [20, 50, 100];

enum Msg {
    Progress(Progress),
    Done(Box<CompressResult>),
    Failed(String),
}

#[derive(Clone)]
struct Selection {
    path: PathBuf,
    info: MediaInfo,
    settings: Settings,
}

enum State {
    Idle,
    Selected(Selection),
    Compressing { percent: f64, settings: Settings },
    Done(CompressResult),
}

struct App {
    state: State,
    target_text: String,
    rx: Option<Receiver<Msg>>,
    cancel: Arc<AtomicBool>,
    status: String,
    error: Option<String>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        // 主题：egui 默认偏好是「跟随系统」，Windows / macOS 由 winit 上报，没问题。
        // 但 winit 在 Linux 上不上报（system_theme() 恒为 None），egui 只好落到
        // fallback_theme —— 它的默认值是暗色，于是浅色桌面上打开也是一片黑。
        // 这里把 Linux 的兜底换成亮色（只影响"系统没告诉我们"的情况，不改偏好本身）。
        #[cfg(target_os = "linux")]
        ctx.options_mut(|o| o.fallback_theme = egui::Theme::Light);

        // egui 默认字体不含中文，不加载就是满屏方框
        let error = match egui_chinese_font::setup_chinese_fonts(ctx) {
            Ok(()) => None,
            Err(e) => Some(format!("中文字体加载失败，界面可能显示方框：{}", e)),
        };
        Self {
            state: State::Idle,
            target_text: "20".to_string(),
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            status: "等待选择文件".to_string(),
            error,
        }
    }

    fn target_bytes(&self) -> Option<u64> {
        let mb: f64 = self.target_text.trim().parse().ok()?;
        if mb <= 0.0 {
            None
        } else {
            Some((mb * 1024.0 * 1024.0).round() as u64)
        }
    }

    fn load(&mut self, path: PathBuf) {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !ALLOWED_EXT.contains(&ext.as_str()) {
            self.error = Some(format!("不支持的格式：{}（支持 mp4 / mov / m4v）", ext));
            return;
        }

        let target = self.target_bytes().unwrap_or(20 * 1024 * 1024);
        match resolve(None).and_then(|tc| {
            let info = probe(&tc, &path)?;
            let settings = plan(&info, target, DEFAULT_AUDIO_KBPS)?.settings();
            Ok(Selection {
                path,
                info,
                settings,
            })
        }) {
            Ok(sel) => {
                self.error = None;
                self.status = format!("已选择 {}", sel.path.display());
                self.state = State::Selected(sel);
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn start(&mut self, path: PathBuf, target_bytes: u64, settings: Settings) {
        let tc = match resolve(None) {
            Ok(tc) => tc,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let output = default_output(&path);
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.cancel.store(false, Ordering::Relaxed);
        self.error = None;
        self.state = State::Compressing {
            percent: 0.0,
            settings,
        };
        self.status = "压缩中…".to_string();

        let cancel = self.cancel.clone();
        let progress_tx: Sender<Msg> = tx.clone();
        std::thread::spawn(move || {
            let sink = FlagSink::new(cancel, move |p: Progress| {
                let _ = progress_tx.send(Msg::Progress(p));
            });
            match compress(
                &CompressConfig {
                    toolchain: &tc,
                    input: &path,
                    output: &output,
                    target_bytes,
                    audio_kbps: DEFAULT_AUDIO_KBPS,
                },
                &sink,
            ) {
                Ok(r) => {
                    let _ = tx.send(Msg::Done(Box::new(r)));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Failed(e.to_string()));
                }
            }
        });
    }

    fn poll(&mut self) {
        // 先收完再处理，避免处理过程中借用 self.rx
        let mut msgs = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
        }
        for msg in msgs {
            match msg {
                Msg::Progress(p) => {
                    if let State::Compressing { percent, .. } = &mut self.state {
                        *percent = p.percent;
                    }
                }
                Msg::Done(r) => {
                    self.status = if r.target_met {
                        format!("目标 ≤{} · 达标", fmt_bytes(r.target_bytes))
                    } else {
                        format!("目标 ≤{} · 未达标", fmt_bytes(r.target_bytes))
                    };
                    self.state = State::Done(*r);
                    self.rx = None;
                }
                Msg::Failed(e) => {
                    self.status = "出错了".to_string();
                    self.error = Some(e);
                    self.state = State::Idle;
                    self.rx = None;
                }
            }
        }
    }

    fn draw_idle(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(30.0);
            egui::Frame::group(ui.style())
                .fill(ui.visuals().extreme_bg_color)
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(340.0, 190.0));
                    ui.vertical_centered(|ui| {
                        ui.add_space(28.0);
                        ui.label(egui::RichText::new("把视频拖到这里").size(18.0));
                        ui.add_space(16.0);
                        if ui.button("点击选择文件").clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .add_filter("视频", &ALLOWED_EXT)
                                .pick_file()
                            {
                                self.load(path);
                            }
                        }
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new("支持 mp4 / mov / m4v").weak());
                    });
                });
        });
    }

    fn draw_selected(&mut self, ui: &mut egui::Ui) {
        let State::Selected(sel) = &self.state else {
            return;
        };
        let sel: Selection = sel.clone();

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(file_name(&sel.path)).strong());
            if ui.small_button("×").clicked() {
                self.state = State::Idle;
                self.status = "等待选择文件".to_string();
            }
        });
        ui.label(format!(
            "{}×{} · {:.0}fps · {} · {}",
            sel.info.width,
            sel.info.height,
            sel.info.fps,
            fmt_duration(sel.info.duration),
            fmt_bytes(sel.info.size_bytes)
        ));
        ui.add_space(12.0);

        ui.label("目标体积");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.target_text)
                    .desired_width(70.0)
                    .hint_text("20"),
            );
            ui.label("MB");
            ui.add_space(8.0);
            for mb in QUICK_SIZES {
                if ui.button(format!("{}", mb)).clicked() {
                    self.target_text = mb.to_string();
                }
            }
        });
        ui.add_space(8.0);

        match self.target_bytes() {
            Some(target) => match plan(&sel.info, target, DEFAULT_AUDIO_KBPS) {
                Ok(ladder) => {
                    let s = ladder.settings();
                    let est =
                        ((s.video_kbps + s.audio_kbps as u64) as f64 * 1000.0 * sel.info.duration
                            / 8.0) as u64;
                    ui.label(format!(
                        "预计：{}×{} {:.0}fps · 约 {}",
                        s.width,
                        s.height,
                        s.fps,
                        fmt_bytes(est)
                    ));
                }
                Err(_) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 60, 60),
                        "目标体积太小，请填更大的值",
                    );
                }
            },
            None => {
                ui.colored_label(
                    egui::Color32::from_rgb(200, 60, 60),
                    "请填写有效的上限（MB）",
                );
            }
        }

        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            let enabled = self.target_bytes().is_some();
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new("开 始 压 缩").min_size(egui::vec2(180.0, 36.0)),
                )
                .clicked()
            {
                if let Some(target) = self.target_bytes() {
                    self.start(sel.path.clone(), target, sel.settings.clone());
                }
            }
        });
    }

    fn draw_compressing(&mut self, ui: &mut egui::Ui) {
        let State::Compressing { percent, settings } = &self.state else {
            return;
        };
        let (percent, settings) = (*percent, settings.clone());

        ui.add_space(20.0);
        ui.add(
            egui::ProgressBar::new((percent as f32 / 100.0).clamp(0.0, 1.0))
                .show_percentage()
                .desired_width(400.0),
        );
        ui.add_space(8.0);
        ui.label(if percent < 50.0 {
            "第 1 遍 / 共 2 遍 · 分析画面"
        } else {
            "第 2 遍 / 共 2 遍 · 正在压缩"
        });
        ui.add_space(12.0);
        ui.label(format!(
            "{}×{} · {:.0}fps",
            settings.width, settings.height, settings.fps
        ));
        ui.label(format!(
            "视频 {} kbps · 音频 {} kbps 单声道",
            settings.video_kbps, settings.audio_kbps
        ));
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            if ui
                .add(egui::Button::new("取 消").min_size(egui::vec2(180.0, 36.0)))
                .clicked()
            {
                self.cancel.store(true, Ordering::Relaxed);
                self.status = "正在取消…".to_string();
            }
        });
    }

    fn draw_done(&mut self, ui: &mut egui::Ui) {
        let State::Done(result) = &self.state else {
            return;
        };
        let r = result.clone();

        ui.add_space(10.0);
        if r.target_met {
            ui.colored_label(
                egui::Color32::from_rgb(40, 150, 80),
                egui::RichText::new("✓ 压缩完成").size(18.0),
            );
        } else {
            ui.colored_label(
                egui::Color32::from_rgb(200, 140, 20),
                egui::RichText::new("⚠ 已压到最小，仍超过上限").size(16.0),
            );
        }
        ui.add_space(8.0);

        ui.label(file_name(&r.output));
        ui.label(format!(
            "{}  →  {}   省 {:.0}%",
            fmt_bytes(r.info.size_bytes),
            fmt_bytes(r.size_bytes),
            if r.info.size_bytes > 0 {
                (1.0 - r.size_bytes as f64 / r.info.size_bytes as f64) * 100.0
            } else {
                0.0
            }
        ));
        ui.add_space(6.0);
        ui.label(format!(
            "{}×{} · {:.0}fps · {} kbps · 音频 {} kbps 单声道",
            r.settings.width,
            r.settings.height,
            r.settings.fps,
            r.settings.video_kbps,
            r.settings.audio_kbps
        ));

        if !r.target_met {
            ui.add_space(12.0);
            ui.label("建议");
            ui.label("  ① 剪掉片头片尾约 30 秒（可省约 20%）");
            ui.label("  ② 拆成两段分别上传");
            let suggested = (r.size_bytes as f64 * 1.2 / (1024.0 * 1024.0)).ceil() as u64;
            ui.horizontal(|ui| {
                ui.label("  ③ 放宽上限");
                if ui.button(format!("改用 {} MB 重试", suggested)).clicked() {
                    self.target_text = suggested.to_string();
                    let target = suggested * 1024 * 1024;
                    let path = r.input.clone();
                    self.load(path);
                    if let State::Selected(sel) = &self.state {
                        let (p, s) = (sel.path.clone(), sel.settings.clone());
                        self.start(p, target, s);
                    }
                }
            });
        }

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if ui.button("打开所在文件夹").clicked() {
                open_in_file_manager(&r.output);
            }
            if ui.button("再压一个").clicked() {
                self.state = State::Idle;
                self.status = "等待选择文件".to_string();
                self.error = None;
            }
        });
    }
}

impl eframe::App for App {
    /// 后台线程在跑，需要持续重绘才能看到进度
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if matches!(self.state, State::Compressing { .. }) {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
        if let Some(file) = dropped.first() {
            let path = file.path().to_path_buf();
            if path.exists() {
                self.load(path);
            }
        }

        // 状态栏必须在 CentralPanel 之前
        egui::Panel::top("status").show(ui, |ui| {
            ui.label(&self.status);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            ui.heading("VZip · 朗读视频压缩");
            ui.separator();
            ui.add_space(8.0);

            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), err);
                ui.add_space(6.0);
            }

            match &self.state {
                State::Idle => self.draw_idle(ui),
                State::Selected(_) => self.draw_selected(ui),
                State::Compressing { .. } => self.draw_compressing(ui),
                State::Done(_) => self.draw_done(ui),
            }
        });
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn default_output(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    let dir = input.parent().unwrap_or(Path::new("."));
    dir.join(format!("{}_vzip.mp4", stem))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Linux 上 winit 的 system_theme() 恒为 None（winit 0.30 platform_impl/linux/mod.rs:909），
    /// egui 便落到 fallback_theme。不能留 egui 默认的暗色，否则浅色桌面每次打开都是黑的。
    #[test]
    fn linux_falls_back_to_light_theme() {
        let ctx = egui::Context::default();
        let _app = App::new(&ctx);
        assert_eq!(ctx.theme(), egui::Theme::Light, "Linux 兜底主题应为亮色");
    }
}

fn open_in_file_manager(path: &Path) {
    let dir = path.parent().unwrap_or(Path::new("."));
    let _ = if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(dir).status()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(dir).status()
    } else {
        std::process::Command::new("xdg-open").arg(dir).status()
    };
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 560.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "VZip",
        options,
        Box::new(|cc| {
            let app = App::new(&cc.egui_ctx);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}
