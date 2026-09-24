use crate::resolve::Toolchain;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;

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
    let out = crate::resolve::command(&tc.ffprobe)
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

    // `encode` 映射的是 `0:a:0`，就是第一条音轨。测时长、判断有没有声都用这一条，
    // 免得"量的是哪条"和"写出去的是哪条"不是同一条
    let audio = streams
        .iter()
        .find(|s| str_field(s, "codec_type") == Some("audio"));

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

    let duration = mapped_duration(video, audio, format)
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
        has_audio: audio.is_some(),
        video_kbps,
        size_bytes: format.and_then(|f| num_field(f, "size")).unwrap_or(0.0) as u64,
    })
}

/// 用来算体积预算的时长：取**实际会被写出去**的那两条流（视频 + 首条音轨）里较长的。
///
/// 为什么不用容器的 `format.duration`：它是所有流里最长的，包含根本没被映射的附加流。
/// 手机视频常带单包 data 轨（运动数据、`apac` 空间音频的伴生轨），一个包就横跨整片时长；
/// 片子被别的工具剪短之后这些轨仍保留原始长度，容器时长于是虚大几十倍，预算被算成负数、
/// 档位静默掉到质量下限（实测：30 秒的片、2MB 目标，只压出 571KB）。反过来只看视频轨也不行：
/// 音轨比视频长时输出的实际时长是音轨，按视频算会低估、预算给大，产出超出目标体积。
///
/// 两条流都拿不到时长时才退回容器时长——TS / 裸流这类容器本来就没有逐流时长。
fn mapped_duration(video: &Value, audio: Option<&Value>, container: Option<&Value>) -> Option<f64> {
    let video_dur = num_field(video, "duration").unwrap_or(0.0);
    let audio_dur = audio.and_then(|a| num_field(a, "duration")).unwrap_or(0.0);

    let mapped = video_dur.max(audio_dur);
    if mapped > 0.0 {
        return Some(mapped);
    }
    container
        .and_then(|f| num_field(f, "duration"))
        .filter(|d| *d > 0.0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 真实的踩坑形状：30 秒的片，容器时长被单包 data 轨顶到 1257.6s。
    /// 预算必须按视频轨的 30.07 算，否则会静默掉到质量下限。
    #[test]
    fn container_duration_from_auxiliary_streams_is_ignored() {
        let video = json!({ "duration": "30.073333" });
        let audio = json!({ "duration": "30.015" });
        let container = json!({ "duration": "1257.634" });

        assert_eq!(
            mapped_duration(&video, Some(&audio), Some(&container)),
            Some(30.073333)
        );
    }

    /// 音轨比视频长时输出的实际时长是音轨：按视频算会低估、产出超目标体积
    #[test]
    fn longer_audio_wins_or_the_output_overshoots_the_limit() {
        let video = json!({ "duration": "30.0" });
        let audio = json!({ "duration": "45.5" });

        assert_eq!(
            mapped_duration(&video, Some(&audio), None),
            Some(45.5),
            "音轨更长时必须按音轨算，否则预算给大、超出上限"
        );
        // 没有音轨就只看视频
        assert_eq!(mapped_duration(&video, None, None), Some(30.0));
    }

    /// 逐流时长缺失（TS / 裸流）才退回容器时长；都没有就没得算，交给调用方报错
    #[test]
    fn container_duration_is_the_last_resort() {
        let no_dur = json!({ "codec_type": "video" });
        let container = json!({ "duration": "12.5" });

        assert_eq!(
            mapped_duration(&no_dur, None, Some(&container)),
            Some(12.5),
            "两条流都没时长时只能信容器"
        );
        // duration 是 0 / "N/A" 这种也当没有
        let zero = json!({ "duration": "0.000000" });
        assert_eq!(mapped_duration(&zero, None, Some(&container)), Some(12.5));
        assert_eq!(mapped_duration(&no_dur, None, None), None);
        assert_eq!(
            mapped_duration(&no_dur, None, Some(&json!({ "duration": "N/A" }))),
            None
        );
    }
}
