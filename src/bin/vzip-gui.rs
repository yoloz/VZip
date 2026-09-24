// 打包出去给用户双击的是 release 版：不加这行，Windows 上双击会先弹一个黑色 cmd
// 窗口。错误不再靠控制台看，改由 logger 落到 vzip-gui.log + 弹窗（见文件末尾）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui;
use eframe::wgpu;
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
            ui.heading("VZip · 视频压缩");
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

// ------------------------------------------------------------ 日志 / 启动失败

/// 自己装 logger，因为 GUI 的启动失败否则会**完全静默**：
/// eframe 把错误只写进 `log::error!`（见 eframe `native/run.rs`），没装 logger
/// 就等于丢进黑洞，而双击运行的 GUI 连控制台都一闪而过——用户只看到"打不开"。
/// 所以落到 exe 同目录的 `vzip-gui.log`（每次运行覆盖，留的就是最近一次），
/// 同时镜像一份到 stderr，终端里跑的时候能直接看。
struct FileLogger {
    level: log::LevelFilter,
    file: Mutex<Option<File>>,
}

impl FileLogger {
    fn new(level: log::LevelFilter, file: Option<File>) -> Self {
        Self {
            level,
            file: Mutex::new(file),
        }
    }

    /// 先落盘、再尽力镜像到 stderr。用 `write_all` 而不是 `eprint!`：
    /// GUI 子系统下 stderr 可能是个无效句柄，`eprint!` 写失败会 panic——
    /// 错误路径上不能再炸一次。
    fn write_line(&self, line: &str) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.write_all(line.as_bytes());
            }
        }
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.write_line(&format!(
                "{:<5} {} — {}\n",
                record.level(),
                record.target(),
                record.args()
            ));
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// 日志落哪：优先 exe 同目录（解压即用的分发包里就是解压目录），
/// 退到当前目录，再退到临时目录；都写不进去就只留 stderr。
fn open_log_file() -> Option<(PathBuf, File)> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("vzip-gui.log"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("vzip-gui.log"));
    }
    candidates.push(std::env::temp_dir().join("vzip-gui.log"));

    candidates
        .into_iter()
        .find_map(|path| File::create(&path).ok().map(|file| (path, file)))
}

/// 级别用 `VZIP_LOG` 覆盖（`info` / `debug` / `trace` / `off`…），默认 info。
///
/// 默认不挂 debug：wgpu / naga 的调试输出动辄几千行（每次启动都写文件），
/// 真出问题的关键结论我们自己用 info 记（启动参数、后端候选、失败原因）。
/// 要追 wgpu 内部的细节，`set VZIP_LOG=debug`（或 `trace`）再跑一次。
fn init_logging() -> Option<PathBuf> {
    let level = std::env::var("VZIP_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(log::LevelFilter::Info);

    let opened = open_log_file();
    let path = opened.as_ref().map(|(path, _)| path.clone());
    let logger = FileLogger::new(level, opened.map(|(_, file)| file));
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
    path
}

/// 启动失败时弹个框：双击运行的场景下这是唯一看得见错误的地方。
/// 弹不出来（比如 Linux 上没装 zenity）也没关系——日志已经落盘了。
fn show_startup_error(message: &str, log_path: Option<&Path>) {
    let mut text = format!("VZip 启动失败：\n\n{message}");
    if let Some(path) = log_path {
        text.push_str(&format!("\n\n详细日志：{}", path.display()));
    }
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("VZip 启动失败")
        .set_description(text)
        .show();
}

fn main() {
    let log_path = init_logging();
    log::info!(
        "vzip-gui {} 启动（exe={:?} 日志={:?}）",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe().ok(),
        log_path
    );

    if let Err(err) = run() {
        log::error!("启动失败：{err}");
        show_startup_error(&err.to_string(), log_path.as_deref());
    }
}

/// Windows 上交给 wgpu 的候选后端；其他平台返回 `None`，即不覆盖 wgpu 自己的默认
/// （`PRIMARY | GL`，macOS 照旧走 Metal）。
///
/// 为什么 Windows 上排掉 Vulkan：有些 Intel 显卡驱动（实测 31.0.101.2141）在
/// `vkCreateDevice` 里直接崩进程，wgpu 连错误都返回不了——日志停在
/// `Supported extensions:` 就断了，用户看到的是"双击一闪而过"。DX12 才是 Windows
/// 上的首选后端，GL 兜底（两者实测都能出窗口）。`WGPU_BACKEND` 永远优先，
/// 纯 Vulkan 的机器 `set WGPU_BACKEND=vulkan` 还能用回来（见 README）。
fn preferred_backends(windows: bool) -> Option<wgpu::Backends> {
    if !windows {
        return None;
    }
    Some(wgpu::Backends::from_env().unwrap_or(wgpu::Backends::DX12 | wgpu::Backends::GL))
}

/// 把候选后端写进 eframe 的配置。单独拎出来是为了能测（见文件末尾）：
/// 光算对候选没用，得确认它真的落到了 eframe 建 instance 时读的那份配置上。
fn set_backends(options: &mut eframe::NativeOptions, backends: Option<wgpu::Backends>) {
    // 无论哪条路径都在 info 上留一句"这次给 wgpu 的是什么后端"：默认级别下就看得见，
    // 不用为了这一句去开 debug（挑中哪个适配器仍然是 wgpu 的 debug 输出）
    let Some(backends) = backends else {
        log::info!("wgpu 候选后端：wgpu 默认（PRIMARY | GL）");
        return;
    };
    match &mut options.wgpu_options.wgpu_setup {
        eframe::egui_wgpu::WgpuSetup::CreateNew(setup) => {
            log::info!("wgpu 候选后端：{backends:?}");
            setup.instance_descriptor.backends = backends;
        }
        // 到不了这儿：eframe 默认就是 CreateNew。真到了说明上游改了默认值，
        // 我们的后端偏好没生效——留条日志，别又变成一次静默失败
        other => log::warn!("wgpu_setup 不是 CreateNew（{other:?}），后端候选没设上"),
    }
}

fn run() -> eframe::Result<()> {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 560.0])
            .with_resizable(false),
        ..Default::default()
    };
    set_backends(&mut options, preferred_backends(cfg!(windows)));
    eframe::run_native(
        "VZip",
        options,
        Box::new(|cc| {
            let app = App::new(&cc.egui_ctx);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
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

#[cfg(test)]
mod log_tests {
    use super::*;
    use log::Log as _; // 调 FileLogger::log 需要 trait 在作用域里

    fn logger_at(dir_tag: &str, level: log::LevelFilter) -> (PathBuf, FileLogger) {
        let dir = std::env::temp_dir().join(format!("vzip-gui-{dir_tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vzip-gui.log");
        let logger = FileLogger::new(level, Some(File::create(&path).unwrap()));
        (path, logger)
    }

    /// 这次改动的核心：eframe 的启动错误必须落进文件。
    /// 下面这句就是 eframe `native/run.rs` 里唯一的错误出口。
    #[test]
    fn eframe_startup_error_reaches_the_log_file() {
        let (path, logger) = logger_at("err", log::LevelFilter::Debug);

        logger.log(
            &log::Record::builder()
                .level(log::Level::Error)
                .target("eframe::native::run")
                .args(format_args!(
                    "Exiting because of error: 没有可用的图形适配器"
                ))
                .build(),
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("没有可用的图形适配器"), "实际内容：{text}");
        assert!(text.contains("eframe::native::run"), "实际内容：{text}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// `VZIP_LOG` 能压级别：排查完就不用再往文件里写噪音。
    #[test]
    fn level_filter_is_respected() {
        let (path, logger) = logger_at("level", log::LevelFilter::Info);

        logger.log(
            &log::Record::builder()
                .level(log::Level::Trace)
                .target("wgpu_core")
                .args(format_args!("这条不该出现"))
                .build(),
        );
        logger.log(
            &log::Record::builder()
                .level(log::Level::Warn)
                .target("wgpu_core")
                .args(format_args!("这条该出现"))
                .build(),
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("这条不该出现"), "实际内容：{text}");
        assert!(text.contains("这条该出现"), "实际内容：{text}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

/// 后端候选是这次修 Windows 起不来的关键，守两条：默认里没有 Vulkan、`WGPU_BACKEND` 优先。
/// 参数化平台而不是 `cfg!`，这样 Linux 上跑 `cargo test` 也能校验 Windows 那条分支。
#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn windows_default_backends_exclude_vulkan() {
        assert_eq!(
            preferred_backends(false),
            None,
            "非 Windows 不该覆盖 wgpu 默认"
        );

        let win = preferred_backends(true).expect("Windows 上必须给一组候选后端");
        if let Some(env) = wgpu::Backends::from_env() {
            assert_eq!(win, env, "WGPU_BACKEND 是用户的逃生舱，必须优先");
        } else {
            assert_eq!(
                win,
                wgpu::Backends::DX12 | wgpu::Backends::GL,
                "Windows 默认候选必须正好是 DX12 + GL（不含 Vulkan）"
            );
        }
    }

    /// 候选算对了还得真的写进 eframe 那份配置——否则等于没改。
    /// 这条只在 Linux 上构造 `NativeOptions`（不开窗口、不碰显卡），所以 CI 里也能跑。
    #[test]
    fn backends_reach_the_eframe_config() {
        let want = wgpu::Backends::DX12 | wgpu::Backends::GL;
        let mut options = eframe::NativeOptions::default();
        set_backends(&mut options, Some(want));

        match &options.wgpu_options.wgpu_setup {
            eframe::egui_wgpu::WgpuSetup::CreateNew(setup) => {
                assert_eq!(setup.instance_descriptor.backends, want);
            }
            other => panic!("eframe 默认应给出 CreateNew，实际 {other:?}"),
        }
    }
}
