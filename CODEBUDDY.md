# CODEBUDDY.md

VZip：给小孩朗读视频打卡用的视频压缩工具。手机拍的朗读视频几百 MB 到 1GB+，平台有体积上限且转码慢常上传失败。

**优先级：人声清晰度 > 视频画质。** 任何取舍先牺牲视频，不动音频。

## 规则

- 编解码一律复用 ffmpeg，不自研
- 调 ffmpeg 走**子进程 CLI**，执行层抽象成 trait（将来可换 FFI）
- 依赖：`anyhow` / `clap` / `serde_json` / `ctrlc` / `eframe` / `egui-chinese-font` / `rfd`
- 两个 bin：`vzip`（CLI）和 `vzip-gui`（egui）
- 内存只有 6G，编 eframe 依赖树要用 `cargo build -j 2`，否则会 OOM 重启
- 子进程参数用 `Command` 参数数组，不拼 shell 字符串（中文、空格路径必须正常）
- **不静默失败**：降级必须在 `CompressResult` 里体现（`target_met` / `reached_floor`），CLI 明确告知
- 临时文件（两遍编码 passlog）用完必删
- GUI 用 **egui**（M1），core 与 UI 解耦。egui 需显式加载中文字体
- 主题：egui 默认「跟随系统」，Windows / macOS 由 winit 上报；但 **winit 在 Linux 上
  不上报**（`system_theme()` 恒为 None），egui 会落到 `fallback_theme`（默认暗色，
  浅色桌面打开就是黑的）→ `App::new` 里只在 Linux 把兜底改成亮色。别删这段
- 若将来要在界面里内嵌播放视频 → 换 Tauri 2（egui 做不到）
- ffmpeg 查找顺序：`--ffmpeg` > `VZIP_FFMPEG` > **exe 同目录（分发自带）** > PATH。
  自带必须优先，否则分发包会被系统旧 ffmpeg 顶掉再卡版本检查
- 分发靠 GitHub Actions（`.github/workflows/`）：推 `v*` tag 出四平台包，
  每平台自带 ffmpeg（eugeneware/ffmpeg-static，GPL，见 `NOTICE.txt`），
  打包前后都有校验 + 冒烟测试，坏包不发
- CI 里的 action 都跑在 **node24**（checkout v7 / upload-artifact v7 /
  action-gh-release v3 / rust-cache v2），GitHub 托管 runner 没问题；
  将来若换自托管 runner 得 ≥ 2.329.0，否则会退回旧运行时
- Linux 上编 egui 要 `libwayland-dev`（winit 的 wayland 后端走 client_system，
  只装 libxkbcommon 会在链接期失败）+ `libxkbcommon-dev`、`libegl1-mesa-dev`、`libgl1-mesa-dev`

## 进度

- [x] 可行性 + 架构决策（`docs/design.md`）
- [x] M0 单文件 CLI：目标体积驱动压缩（`cargo build` 后 `vzip 输入.mp4 --max-size 20MB`）
- [x] M1 GUI（egui）+ 四平台打包：`cargo run --bin vzip-gui`；界面草稿见 `docs/ui-sketch.md`；
  CI 出 linux-x64 / windows-x64 / macos-arm64 / macos-x64 四个包，各带 ffmpeg（`v*` tag 触发）
- [ ] M2 引擎增强：预设 + 批量 + 硬件编码 + 取样试压 + 剪片头片尾
