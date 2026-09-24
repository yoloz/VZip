pub mod encode;
pub mod plan;
pub mod probe;
pub mod resolve;

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use encode::{encode, ProgressSink};
use plan::{plan, Ladder, Settings};
use probe::{probe, MediaInfo};
use resolve::Toolchain;

/// 音频默认单声道 96kbps：人声足够，比手机默认立体声省 2~4 倍
pub const DEFAULT_AUDIO_KBPS: u32 = 96;

pub fn fmt_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

pub fn fmt_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{}小时{}分{}秒", h, m, s)
    } else if m > 0 {
        format!("{}分{}秒", m, s)
    } else {
        format!("{}秒", s)
    }
}
/// 降级最多 8 次（含初始档）
const MAX_ATTEMPTS: u32 = 8;

pub struct CompressConfig<'a> {
    pub toolchain: &'a Toolchain,
    pub input: &'a Path,
    pub output: &'a Path,
    pub target_bytes: u64,
    pub audio_kbps: u32,
}

#[derive(Debug, Clone)]
pub struct CompressResult {
    pub input: PathBuf,
    pub output: PathBuf,
    pub size_bytes: u64,
    pub target_bytes: u64,
    /// 是否压到目标体积以内
    pub target_met: bool,
    /// 是否已撞到质量地板（再压就要糊了）
    pub reached_floor: bool,
    pub attempts: u32,
    pub settings: Settings,
    pub info: MediaInfo,
}

/// 目标体积驱动压缩：探测 → 算档 → 两遍编码 → 校验 → 撞墙就降档重试
pub fn compress(cfg: &CompressConfig, sink: &dyn ProgressSink) -> Result<CompressResult> {
    let info = probe(cfg.toolchain, cfg.input)?;
    let mut ladder: Ladder = plan(&info, cfg.target_bytes, cfg.audio_kbps)?;

    if let Some(parent) = cfg.output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("无法创建输出目录：{:?}", parent))?;
        }
    }

    let mut attempts = 0u32;
    let mut target_met = false;
    let mut reached_floor = false;
    let mut settings = ladder.settings();

    while attempts < MAX_ATTEMPTS {
        attempts += 1;
        settings = ladder.settings();
        encode(cfg.toolchain, cfg.input, cfg.output, &settings, &info, sink)?;

        let size = fs::metadata(cfg.output)
            .with_context(|| format!("读取输出文件失败：{}", cfg.output.display()))?
            .len();

        if size <= cfg.target_bytes {
            target_met = true;
            break;
        }

        if !ladder.degrade() {
            reached_floor = true;
            break;
        }
    }

    let size_bytes = fs::metadata(cfg.output).map(|m| m.len()).unwrap_or(0);

    Ok(CompressResult {
        input: cfg.input.to_path_buf(),
        output: cfg.output.to_path_buf(),
        size_bytes,
        target_bytes: cfg.target_bytes,
        target_met,
        reached_floor,
        attempts,
        settings,
        info,
    })
}
