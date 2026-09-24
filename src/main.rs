use anyhow::{anyhow, bail, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use vzip::encode::{FlagSink, Progress};
use vzip::resolve::resolve;
use vzip::{compress, fmt_bytes, CompressConfig, DEFAULT_AUDIO_KBPS};

#[derive(Parser, Debug)]
#[command(name = "vzip", about = "把手机拍的朗读视频压到指定体积以内")]
struct Args {
    /// 输入视频
    input: PathBuf,

    /// 目标体积上限，如 20MB / 200MB / 52428800
    #[arg(long, value_name = "SIZE")]
    max_size: String,

    /// 输出文件，默认在同目录生成 原名_vzip.mp4
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// 音频码率（kbps），默认 96 单声道
    #[arg(long, value_name = "KBPS", default_value_t = DEFAULT_AUDIO_KBPS)]
    audio_kbps: u32,

    /// 指定 ffmpeg 路径
    #[arg(long, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,

    /// 不显示进度
    #[arg(long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let target_bytes = parse_size(&args.max_size)?;

    if !args.input.exists() {
        bail!("输入文件不存在：{}", args.input.display());
    }

    let output = match &args.output {
        Some(p) => p.clone(),
        None => default_output(&args.input),
    };
    if output == args.input {
        bail!("输出路径不能和输入相同（会覆盖原片）");
    }

    let toolchain = resolve(args.ffmpeg.as_deref())?;

    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
        .map_err(|e| anyhow!("无法设置 Ctrl+C 处理：{}", e))?;

    let quiet = args.quiet;
    let sink = FlagSink::new(cancelled.clone(), move |p: Progress| {
        if !quiet {
            eprint!("\r压缩中 {:5.1}%", p.percent);
        }
    });

    let result = compress(
        &CompressConfig {
            toolchain: &toolchain,
            input: &args.input,
            output: &output,
            target_bytes,
            audio_kbps: args.audio_kbps,
        },
        &sink,
    )?;

    if !quiet {
        eprint!("\r{:>12}\r", "");
    }

    println!("✓ 已输出：{}", result.output.display());
    println!(
        "  目标 ≤{} ｜ 实际 {} ｜ {}",
        fmt_bytes(result.target_bytes),
        fmt_bytes(result.size_bytes),
        if result.target_met {
            "达标".to_string()
        } else {
            "未达标".to_string()
        }
    );
    println!(
        "  参数：{}x{} {:.0}fps ｜ 视频 {} kbps ｜ 音频 {} kbps 单声道 ｜ 尝试 {} 次",
        result.settings.width,
        result.settings.height,
        result.settings.fps,
        result.settings.video_kbps,
        result.settings.audio_kbps,
        result.attempts
    );

    if !result.target_met {
        println!(
            "  原因：{}",
            if result.reached_floor {
                "已达质量下限（480p / 20fps），继续压会看不清画面"
            } else {
                "已尝试降级仍未达标"
            }
        );
        println!("  建议：① 剪掉片头片尾约 30 秒（可省约 20%）");
        println!("        ② 拆成两段分别上传");
        println!(
            "        ③ 放宽上限（当前 {}，实际需要约 {}）",
            fmt_bytes(result.target_bytes),
            fmt_bytes(result.size_bytes)
        );
    }

    Ok(())
}

fn default_output(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    let dir = input.parent().unwrap_or(Path::new("."));
    dir.join(format!("{}_vzip.mp4", stem))
}

/// "20MB" / "20M" / "20mb" / "20000000"，按 1024 进制
fn parse_size(text: &str) -> Result<u64> {
    let s = text.trim().replace([' ', ','], "");
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num
        .parse()
        .map_err(|_| anyhow!("无法解析体积：{}（示例：20MB）", text))?;
    let multiplier = match unit.to_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => bail!("无法识别的体积单位：{}（支持 KB / MB / GB）", unit),
    };
    let bytes = (value * multiplier).round() as u64;
    if bytes == 0 {
        bail!("目标体积必须大于 0");
    }
    Ok(bytes)
}
