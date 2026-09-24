use crate::probe::MediaInfo;
use anyhow::{bail, Result};

/// bpp = 视频码率 ÷ (宽 × 高 × 帧率)。头肩朗读这类低运动画面的经验区间
pub const TARGET_BPP: f64 = 0.05;
pub const MIN_BPP: f64 = 0.02;
/// 留给容器头 + 编码器浮动
pub const SAFETY: f64 = 0.97;
/// 质量地板：480p 长边
pub const FLOOR_LONG_SIDE: u32 = 854;

const RES_LADDER: [u32; 4] = [1920, 1280, 960, FLOOR_LONG_SIDE];
const FPS_LADDER: [f64; 3] = [30.0, 24.0, 20.0];
const MIN_VIDEO_KBPS: f64 = 50.0;

#[derive(Debug, Clone)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub video_kbps: u64,
    pub audio_kbps: u32,
    pub bpp: f64,
}

/// 分辨率/帧率阶梯。降档顺序：码率 → 帧率 → 分辨率
#[derive(Debug, Clone)]
pub struct Ladder {
    res: Vec<u32>,
    fps: Vec<f64>,
    res_idx: usize,
    fps_idx: usize,
    mult: f64,
    base_kbps: f64,
    audio_kbps: u32,
    src_w: u32,
    src_h: u32,
    step: usize,
}

impl Ladder {
    pub fn settings(&self) -> Settings {
        let (width, height) = scale_dims(self.src_w, self.src_h, self.res[self.res_idx]);
        let fps = self.fps[self.fps_idx];
        let kbps = (self.base_kbps * self.mult).max(MIN_VIDEO_KBPS);
        Settings {
            width,
            height,
            fps,
            video_kbps: kbps.round() as u64,
            audio_kbps: self.audio_kbps,
            bpp: kbps * 1000.0 / (width as f64 * height as f64 * fps),
        }
    }

    /// 降到下一档。返回 false 表示已经撞到质量地板
    pub fn degrade(&mut self) -> bool {
        loop {
            if self.step >= 7 {
                return false;
            }
            let step = self.step;
            self.step += 1;
            let moved = match step {
                0 => {
                    self.mult *= 0.90;
                    true
                }
                1 => self.lower_fps(),
                2 => self.lower_res(),
                3 => {
                    self.mult *= 0.85;
                    true
                }
                4 => self.lower_res(),
                5 => self.lower_fps(),
                6 => self.lower_res(),
                _ => false,
            };
            if moved {
                return true;
            }
        }
    }

    fn lower_fps(&mut self) -> bool {
        if self.fps_idx + 1 < self.fps.len() {
            self.fps_idx += 1;
            true
        } else {
            false
        }
    }

    fn lower_res(&mut self) -> bool {
        if self.res_idx + 1 < self.res.len() {
            self.res_idx += 1;
            true
        } else {
            false
        }
    }
}

pub fn plan(info: &MediaInfo, target_bytes: u64, audio_kbps: u32) -> Result<Ladder> {
    let duration = info.duration;
    if duration <= 0.0 {
        bail!("视频时长为 0，无法计算码率预算");
    }

    let total_bits = target_bytes as f64 * 8.0;
    let audio_bits = audio_kbps as f64 * 1000.0 * duration;
    let budget_kbps = (total_bits * SAFETY - audio_bits) / duration / 1000.0;

    // 预算过宽时不要用满（填满只会更大更慢）；预算过窄也不报错，
    // 兜到最低码率，让它一路降到质量地板，最后由调用方报告"未达标"
    let cap_kbps = info.video_kbps.map(|b| b * 0.8);
    let base_kbps = match cap_kbps {
        Some(cap) if cap < budget_kbps => cap,
        _ => budget_kbps,
    }
    .max(MIN_VIDEO_KBPS);

    let src_long = info.width.max(info.height);
    let mut res: Vec<u32> = vec![src_long];
    res.extend(RES_LADDER.iter().filter(|&&r| r < src_long).copied());
    res.sort_unstable_by(|a, b| b.cmp(a));
    res.dedup();

    let mut fps: Vec<f64> = vec![info.fps.min(FPS_LADDER[0])];
    for f in FPS_LADDER.iter().skip(1) {
        if *f < fps[0] {
            fps.push(*f);
        }
    }

    // 从高到低取第一个 bpp 达标的组合；都不达标就用最低档
    let mut pick = (res.len() - 1, fps.len() - 1);
    'outer: for (ri, &long) in res.iter().enumerate() {
        for (fi, &f) in fps.iter().enumerate() {
            let (w, h) = scale_dims(info.width, info.height, long);
            let bpp = base_kbps * 1000.0 / (w as f64 * h as f64 * f);
            if bpp >= TARGET_BPP {
                pick = (ri, fi);
                break 'outer;
            }
        }
    }

    Ok(Ladder {
        res,
        fps,
        res_idx: pick.0,
        fps_idx: pick.1,
        mult: 1.0,
        base_kbps,
        audio_kbps,
        src_w: info.width,
        src_h: info.height,
        step: 0,
    })
}

/// 按长边缩放，另一边保持比例并取偶数
fn scale_dims(src_w: u32, src_h: u32, long: u32) -> (u32, u32) {
    let src_long = src_w.max(src_h);
    if src_long == 0 || long >= src_long {
        return (even(src_w), even(src_h));
    }
    let scale = long as f64 / src_long as f64;
    let w = (src_w as f64 * scale).round() as u32;
    let h = (src_h as f64 * scale).round() as u32;
    (even(w.max(2)), even(h.max(2)))
}

fn even(v: u32) -> u32 {
    if v.is_multiple_of(2) {
        v
    } else {
        v + 1
    }
}
