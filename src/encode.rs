use crate::plan::Settings;
use crate::probe::MediaInfo;
use crate::resolve::Toolchain;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    /// 0~100，含两遍编码的整体进度
    pub percent: f64,
    pub seconds: f64,
}

pub trait ProgressSink {
    fn report(&self, progress: Progress);
    fn cancelled(&self) -> bool;
}

pub struct NoopSink;
impl ProgressSink for NoopSink {
    fn report(&self, _: Progress) {}
    fn cancelled(&self) -> bool {
        false
    }
}

/// 用 ctor 的取消开关
pub struct Cancellable {
    flag: Arc<AtomicBool>,
}

impl Cancellable {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }
}

impl Default for Cancellable {
    fn default() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub struct FlagSink<F: Fn(Progress) + Send + Sync> {
    flag: Arc<AtomicBool>,
    on_progress: F,
}

impl<F: Fn(Progress) + Send + Sync> FlagSink<F> {
    pub fn new(flag: Arc<AtomicBool>, on_progress: F) -> Self {
        Self { flag, on_progress }
    }
}

impl<F: Fn(Progress) + Send + Sync> ProgressSink for FlagSink<F> {
    fn report(&self, p: Progress) {
        (self.on_progress)(p)
    }
    fn cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// 两遍编码的临时目录，析构时自动清理
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("vzip-{}-{}", std::process::id(), nanos));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
    pub fn passlog(&self) -> PathBuf {
        self.path.join("vzip-pass")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 两遍 ABR 编码。依赖 ffmpeg 默认的自动旋转（-autorotate），
/// 所以滤镜链里看到的就是显示方向，缩放直接用显示尺寸
pub fn encode(
    tc: &Toolchain,
    input: &Path,
    output: &Path,
    settings: &Settings,
    info: &MediaInfo,
    sink: &dyn ProgressSink,
) -> Result<()> {
    let encoder = tc
        .pick_h264_encoder()
        .context("当前 ffmpeg 没有任何 H.264 编码器")?;
    let tmp = TempDir::new()?;

    let filter = format!(
        "scale={}:{},fps={}",
        settings.width, settings.height, settings.fps
    );
    let bitrate = format!("{}k", settings.video_kbps);
    let passlog = tmp.passlog();

    // ---- pass 1：只分析不输出 ----
    let pass1: Vec<OsString> = vec![
        OsString::from("-y"),
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
        OsString::from("-i"),
        OsString::from(input),
        OsString::from("-vf"),
        OsString::from(&filter),
        OsString::from("-an"),
        OsString::from("-c:v"),
        OsString::from(encoder),
        OsString::from("-preset"),
        OsString::from("medium"),
        OsString::from("-b:v"),
        OsString::from(&bitrate),
        OsString::from("-pass"),
        OsString::from("1"),
        OsString::from("-passlogfile"),
        OsString::from(&passlog),
        OsString::from("-f"),
        OsString::from("null"),
        OsString::from("-"),
    ];
    run(tc, &pass1, info.duration, sink, 0.0, 50.0)?;

    // ---- pass 2：实际输出 ----
    let mut pass2: Vec<OsString> = vec![
        OsString::from("-y"),
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-progress"),
        OsString::from("pipe:1"),
        OsString::from("-nostats"),
        OsString::from("-i"),
        OsString::from(input),
        OsString::from("-vf"),
        OsString::from(&filter),
        OsString::from("-map"),
        OsString::from("0:v:0"),
        OsString::from("-c:v"),
        OsString::from(encoder),
        OsString::from("-preset"),
        OsString::from("medium"),
        OsString::from("-b:v"),
        OsString::from(&bitrate),
        OsString::from("-pass"),
        OsString::from("2"),
        OsString::from("-passlogfile"),
        OsString::from(&passlog),
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
    ];

    if info.has_audio {
        pass2.extend([
            OsString::from("-map"),
            OsString::from("0:a:0"),
            OsString::from("-c:a"),
            OsString::from("aac"),
            OsString::from("-b:a"),
            OsString::from(format!("{}k", settings.audio_kbps)),
            OsString::from("-ac"),
            OsString::from("1"),
        ]);
    } else {
        pass2.push(OsString::from("-an"));
    }

    pass2.extend([
        OsString::from("-movflags"),
        OsString::from("+faststart"),
        OsString::from(output),
    ]);
    run(tc, &pass2, info.duration, sink, 50.0, 100.0)?;

    Ok(())
}

/// 执行 ffmpeg，把 out_time_us 映射成 [lo, hi] 区间的百分比
fn run(
    tc: &Toolchain,
    args: &[OsString],
    duration: f64,
    sink: &dyn ProgressSink,
    lo: f64,
    hi: f64,
) -> Result<()> {
    let mut child = Command::new(&tc.ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 ffmpeg 失败")?;

    let stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let err_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    let mut cancelled = false;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if let Some(value) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = value.trim().parse::<f64>() {
                let seconds = us / 1_000_000.0;
                let frac = if duration > 0.0 {
                    (seconds / duration).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                sink.report(Progress {
                    percent: lo + (hi - lo) * frac,
                    seconds,
                });
            }
        }
        if sink.cancelled() {
            cancelled = true;
            let _ = child.kill();
            break;
        }
    }

    let status = child.wait()?;
    let err = err_thread.join().unwrap_or_default();

    if cancelled {
        bail!("已取消");
    }
    if !status.success() {
        bail!(
            "ffmpeg 执行失败：{}",
            err.lines().last().unwrap_or("未知错误").trim()
        );
    }
    Ok(())
}
