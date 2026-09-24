use crate::plan::Settings;
use crate::probe::MediaInfo;
use crate::resolve::Toolchain;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
        // 必须和 pass 2 用同一个像素格式：10 bit 源（手机拍的 HDR，HEVC Main 10）
        // 不指定的话 pass 1 就按 10 bit 跑，stats 记 bitdepth=10，pass 2 的
        // `-pix_fmt yuv420p` 会让 libx264 直接拒绝打开编码器（different bitdepth
        // setting than first pass），一帧都进不了 mp4，报错只剩 muxer 那句
        // `Nothing was written into output file…`
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
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
    let mut child = crate::resolve::command(&tc.ffmpeg)
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
        // ffmpeg 的报错常常是"前因 + 收尾总结"两段，只留最后一行等于把前因丢了
        // （`Nothing was written into output file…` 就是典型的收尾总结，光看它无从下手）。
        // 给用户看尾部若干行；完整 stderr 和实际命令行落到日志里。
        log::error!(
            "ffmpeg 失败（{status}）\n  命令：{}\n  stderr：\n{err}",
            command_line(&tc.ffmpeg, args)
        );
        bail!(
            "ffmpeg 执行失败（{status}）：\n{}",
            tail_lines(&err, ERROR_TAIL_LINES)
        );
    }
    Ok(())
}

/// 给用户看的 stderr 尾部行数：够看到前因，又不至于把弹窗撑爆
const ERROR_TAIL_LINES: usize = 15;

/// 实际执行了什么命令——排查时必须有（少了它连 scale、码率都无从对证）
fn command_line(program: &Path, args: &[OsString]) -> String {
    let mut line = program.display().to_string();
    for arg in args {
        line.push(' ');
        line.push_str(&arg.to_string_lossy());
    }
    line
}

/// stderr 的最后 `n` 行（跳过空行），全空则给个明确的占位
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return "（ffmpeg 没有输出任何错误信息）".to_string();
    }
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 报错信息不能再只留最后一行：前因（前面的行）比收尾总结重要。
    /// 之前 `ffmpeg 执行失败：Nothing was written into output file…` 就是这么来的。
    #[test]
    fn error_tail_keeps_the_cause_lines() {
        let stderr = "  前因：某处出错了  \n\n收尾总结\n";
        assert_eq!(tail_lines(stderr, 15), "  前因：某处出错了  \n收尾总结");
        assert_eq!(tail_lines(stderr, 1), "收尾总结");
    }

    #[test]
    fn empty_stderr_says_so() {
        assert_eq!(tail_lines("", 15), "（ffmpeg 没有输出任何错误信息）");
        assert_eq!(
            tail_lines(" \n\n\t\n", 15),
            "（ffmpeg 没有输出任何错误信息）"
        );
    }

    /// 命令行要带上程序路径和每个参数，且有空格/中文的路径也不能丢
    #[test]
    fn command_line_includes_program_and_args() {
        let args = vec![
            OsString::from("-i"),
            OsString::from("/tmp/我的 视频.MOV"),
            OsString::from("-b:v"),
            OsString::from("195k"),
        ];
        assert_eq!(
            command_line(Path::new("/tmp/bundled/ffmpeg"), &args),
            "/tmp/bundled/ffmpeg -i /tmp/我的 视频.MOV -b:v 195k"
        );
    }
}
