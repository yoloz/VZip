use anyhow::{anyhow, bail, Result};
use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const MIN_VERSION: (u32, u32) = (4, 4);

#[derive(Debug, Clone)]
pub struct Toolchain {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub version: (u32, u32),
    encoders: HashSet<String>,
}

impl Toolchain {
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders.contains(name)
    }

    /// 优先软件 x264（画质/码率控制最稳），其次硬件编码器
    pub fn pick_h264_encoder(&self) -> Option<&'static str> {
        [
            "libx264",
            "h264_nvenc",
            "h264_qsv",
            "h264_videotoolbox",
            "h264_vaapi",
        ]
        .into_iter()
        .find(|name| self.has_encoder(name))
    }
}

/// 起 ffmpeg / ffprobe 子进程统一走这里。
///
/// Windows 上必须加 `CREATE_NO_WINDOW`：release 的 `vzip-gui` 是 GUI 子系统进程
/// （`windows_subsystem = "windows"`），它起控制台程序时 Windows 会**给子进程单开一个
/// 控制台窗口**——就是用户看到的那个标题为 ffmpeg.exe 路径的黑窗。子进程的 stdio
/// 全部走管道，根本不需要控制台。
pub(crate) fn command(program: &Path) -> Command {
    let cmd = Command::new(program);
    // Windows 上才需要改，别的平台原样返回（`mut` 只在 cfg 里用，所以用 shadow 而不是 `let mut`）
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = cmd;
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    };
    cmd
}

/// 定位 ffmpeg / ffprobe。
/// 顺序：显式指定 > 环境变量 > exe 同目录（分发包自带的）> PATH
///
/// 自带优先于 PATH：分发包里 ffmpeg 就躺在 exe 旁边，必须先用它，
/// 否则会被系统里更旧的 ffmpeg 顶掉（然后卡在版本检查上）。
pub fn resolve(explicit: Option<&Path>) -> Result<Toolchain> {
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let exe_dir = exe_dir.as_deref();

    let ffmpeg = explicit
        .map(Path::to_path_buf)
        .or_else(|| env_override("ffmpeg"))
        .or_else(|| locate("ffmpeg", None, exe_dir))
        .ok_or_else(|| {
            anyhow!("未找到 ffmpeg\n  请安装 ffmpeg，或用 --ffmpeg 指定路径，或把 ffmpeg 放在 vzip 同目录")
        })?;

    let ffprobe = sibling(&ffmpeg, "ffprobe")
        .or_else(|| env_override("ffprobe"))
        .or_else(|| locate("ffprobe", None, exe_dir))
        .ok_or_else(|| anyhow!("找到了 ffmpeg 但没有 ffprobe：{}", ffmpeg.display()))?;

    let version = version_of(&ffmpeg)?;
    if version < MIN_VERSION {
        bail!(
            "ffmpeg 版本过低：{}.{}，需要 ≥ {}.{}",
            version.0,
            version.1,
            MIN_VERSION.0,
            MIN_VERSION.1
        );
    }

    let encoders = encoders_of(&ffmpeg)?;

    Ok(Toolchain {
        ffmpeg,
        ffprobe,
        version,
        encoders,
    })
}

fn exe_name(name: &str) -> String {
    format!("{}{}", name, env::consts::EXE_SUFFIX)
}

/// VZIP_FFMPEG / VZIP_FFPROBE
fn env_override(name: &str) -> Option<PathBuf> {
    env::var_os(format!("VZIP_{}", name.to_uppercase())).map(PathBuf::from)
}

/// 按优先级取第一个存在的：显式指定 > exe 同目录 > PATH（显式路径不做存在性检查）
fn locate(name: &str, explicit: Option<PathBuf>, exe_dir: Option<&Path>) -> Option<PathBuf> {
    explicit
        .or_else(|| exe_dir.and_then(|dir| sibling_in(dir, name)))
        .or_else(|| find_in_path(name))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let target = exe_name(name);
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(&target))
        .find(|p| p.is_file())
}

fn sibling_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let p = dir.join(exe_name(name));
    p.is_file().then_some(p)
}

fn sibling(beside: &Path, name: &str) -> Option<PathBuf> {
    sibling_in(beside.parent()?, name)
}

/// "ffmpeg version 5.1.9-0+deb12u1 Copyright ..." 或 "ffmpeg version n4.4.1"
fn version_of(ffmpeg: &Path) -> Result<(u32, u32)> {
    let out = command(ffmpeg).arg("-version").output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("");
    let token = first
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_start_matches('n');
    let mut parts = token.split('.');
    let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((major, minor))
}

fn encoders_of(ffmpeg: &Path) -> Result<HashSet<String>> {
    let out = command(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut set = HashSet::new();
    // 跳过 "Encoders:" 标题行和 "------" 分隔行，取每行第二个字段
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 && !fields[0].starts_with('-') {
            set.insert(fields[1].to_string());
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let dir = env::temp_dir().join(format!("vzip-resolve-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, sub: &str, name: &str) -> PathBuf {
            let dir = self.0.join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(exe_name(name));
            std::fs::write(&p, b"").unwrap();
            p
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 分发包自带 ffmpeg 时必须赢过 PATH——否则会被系统旧版顶掉再卡版本检查
    #[test]
    fn exe_dir_wins_over_path() {
        let tmp = TmpDir::new("exe-first");
        let bundled = tmp.file("exe", "ffmpeg");
        tmp.file("path", "ffmpeg"); // 模拟系统里也有一份

        // 注意：本机 PATH 里通常也有 ffmpeg，这里正是要验证它不会抢先
        let found = locate("ffmpeg", None, Some(&tmp.0.join("exe"))).unwrap();
        assert_eq!(found, bundled);
    }

    #[test]
    fn explicit_wins_over_exe_dir() {
        let tmp = TmpDir::new("explicit");
        tmp.file("exe", "ffmpeg");
        let given = PathBuf::from("/opt/custom/ffmpeg");

        let found = locate("ffmpeg", Some(given.clone()), Some(&tmp.0.join("exe"))).unwrap();
        assert_eq!(found, given);
    }

    #[test]
    fn missing_files_are_skipped() {
        let tmp = TmpDir::new("missing");
        assert_eq!(sibling_in(&tmp.0.join("nope"), "ffmpeg"), None);
        // 没有 exe 同目录、PATH 里也找不到时返回 None（用不存在的名字避免撞上真 ffmpeg）
        assert_eq!(locate("vzip-nonexistent-tool", None, None), None);
    }
}
