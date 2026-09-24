use crate::resolve::Toolchain;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// 探测结果。宽高是**显示方向**的尺寸（已按旋转交换过）
#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// 归一化到 0 / 90 / 180 / 270
    pub rotation: f64,
    pub has_audio: bool,
    pub video_kbps: Option<f64>,
    pub size_bytes: u64,
}

pub fn probe(tc: &Toolchain, input: &Path) -> Result<MediaInfo> {
    let out = Command::new(&tc.ffprobe)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
        .arg(input)
        .output()
        .with_context(|| format!("运行 ffprobe 失败：{}", input.display()))?;

    if !out.status.success() {
        bail!("无法读取媒体文件：{}", input.display());
    }

    let root: Value = serde_json::from_slice(&out.stdout)?;
    let format = root.get("format");
    let streams = root
        .get("streams")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    let video = streams
        .iter()
        .find(|s| str_field(s, "codec_type") == Some("video"))
        .ok_or_else(|| anyhow::anyhow!("文件中没有视频轨：{}", input.display()))?;

    let rotation = detect_rotation(video);
    let (mut width, mut height) = (
        num_field(video, "width").unwrap_or(0.0) as u32,
        num_field(video, "height").unwrap_or(0.0) as u32,
    );
    if (rotation - 90.0).abs() < 1.0 || (rotation - 270.0).abs() < 1.0 {
        std::mem::swap(&mut width, &mut height);
    }
    if width == 0 || height == 0 {
        bail!("无法获取视频分辨率：{}", input.display());
    }

    let duration = format
        .and_then(|f| num_field(f, "duration"))
        .or_else(|| num_field(video, "duration"))
        .filter(|d| *d > 0.0)
        .ok_or_else(|| anyhow::anyhow!("无法获取视频时长：{}", input.display()))?;

    let fps = parse_rate(video, "avg_frame_rate")
        .or_else(|| parse_rate(video, "r_frame_rate"))
        .filter(|f| *f > 0.0)
        .unwrap_or(25.0);

    let video_kbps = num_field(video, "bit_rate")
        .or_else(|| format.and_then(|f| num_field(f, "bit_rate")))
        .map(|b| b / 1000.0);

    Ok(MediaInfo {
        duration,
        width,
        height,
        fps,
        rotation,
        has_audio: streams
            .iter()
            .any(|s| str_field(s, "codec_type") == Some("audio")),
        video_kbps,
        size_bytes: format.and_then(|f| num_field(f, "size")).unwrap_or(0.0) as u64,
    })
}

/// 旋转信息可能在 displaymatrix 的 side_data，也可能在 rotate 标签里
fn detect_rotation(video: &Value) -> f64 {
    let mut rot = 0.0f64;
    if let Some(list) = video.get("side_data_list").and_then(|v| v.as_array()) {
        for sd in list {
            if let Some(r) = num_field(sd, "rotation") {
                rot = r;
                break;
            }
        }
    }
    if rot == 0.0 {
        if let Some(tag) = video
            .get("tags")
            .and_then(|t| t.get("rotate"))
            .and_then(|v| v.as_str())
        {
            rot = tag.parse().unwrap_or(0.0);
        }
    }
    ((rot % 360.0) + 360.0) % 360.0
}

fn parse_rate(stream: &Value, key: &str) -> Option<f64> {
    let s = str_field(stream, key)?;
    let mut parts = s.split('/');
    let num: f64 = parts.next()?.parse().ok()?;
    let den: f64 = parts.next()?.parse().ok()?;
    if den == 0.0 {
        None
    } else {
        Some(num / den)
    }
}

fn num_field(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| x.as_f64()).or_else(|| {
        v.get(key)
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok())
    })
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}
