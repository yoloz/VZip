# CODEBUDDY.md

VZip：给小孩朗读视频打卡用的视频压缩工具。手机拍的朗读视频几百 MB 到 1GB+，平台有体积上限且转码慢常上传失败。

**优先级：人声清晰度 > 视频画质。** 任何取舍先牺牲视频，不动音频。

## 规则

- 编解码一律复用 ffmpeg，不自研
- 调 ffmpeg 走**子进程 CLI**，执行层抽象成 trait（将来可换 FFI）
- 依赖：`anyhow` / `clap` / `serde_json` / `ctrlc` / `eframe` / `egui-chinese-font` / `log` / `rfd`
- 两个 bin：`vzip`（CLI）和 `vzip-gui`（egui）
- 内存只有 6G，编 eframe 依赖树要用 `cargo build -j 2`，否则会 OOM 重启
- 子进程参数用 `Command` 参数数组，不拼 shell 字符串（中文、空格路径必须正常）
- **起 ffmpeg / ffprobe 一律走 `resolve::command()`**，别直接 `Command::new`：它在 Windows 上
  带 `CREATE_NO_WINDOW`。release 的 `vzip-gui` 是 GUI 子系统进程
  （`windows_subsystem = "windows"`），起控制台程序时 Windows 会**给子进程单开一个控制台
  窗口**——就是用户看到的那个标题为 ffmpeg.exe 路径的黑窗（子进程 stdio 全走管道，根本不需要
  控制台）。新增子进程调用点漏掉这个 helper 就会复现。别删这段
- **不静默失败**：降级必须在 `CompressResult` 里体现（`target_met` / `reached_floor`），CLI 明确告知
- **失败时的错误信息留 stderr 尾部（`ERROR_TAIL_LINES` = 15 行），不是只留最后一行**：
  ffmpeg 的报错常是"前因 + 收尾总结"两段，最后一行往往是最没信息量的那句总结
  （`Nothing was written into output file…` 就是这么把真正的 bitdepth 冲突藏起来的）。
  完整 stderr 和实际执行的命令行（`command_line`）用 `log::error!` 落日志给排查用，
  弹窗只给尾部若干行。别删这段
- 临时文件（两遍编码 passlog）用完必删
- **两遍编码的像素格式必须两边一致**：pass 1 也要写 `-pix_fmt yuv420p`。10 bit 源
  （手机拍的 HDR 都是 HEVC Main 10 / `yuv420p10le`）不指定就按 10 bit 跑，stats 文件记
  `bitdepth=10`，pass 2 的 `-pix_fmt yuv420p` 会让 libx264 以 `different bitdepth
  setting than first pass` 拒绝打开编码器——一帧都进不了 mp4，报错只剩 muxer 那句
  `Nothing was written into output file…`（那是收尾总结不是原因，前因在 stderr 前几行，
  所以失败必须保留 stderr 尾部而不是只留最后一行）。`testdata` 里都是 8 bit SDR 片，
  本地测试测不出来。别删这段
- **体积预算用的时长取 `max(视频轨, 首条音轨)`**（`probe::mapped_duration`），不能用
  `format.duration`：它是**所有**流里最长的，含根本没被映射的附加流——手机视频的单包 data 轨
  （运动数据 / `apac` 空间音频的伴生轨）一个包就横跨整片时长，片子被别的工具剪短后它们仍保留
  原长，容器时长于是虚大几十倍，预算被算成负数、档位**静默**掉到质量下限（实测：30 秒的片、
  2MB 目标，只压出 571KB）。也不能只看视频轨：音轨比视频长时输出的实际时长是音轨，按视频算
  会低估、产出超出目标体积。两条流都拿不到时长（TS / 裸流）才退回容器时长。别删这段
- GUI 用 **egui**（M1），core 与 UI 解耦。egui 需显式加载中文字体
- 主题：egui 默认「跟随系统」，Windows / macOS 由 winit 上报；但 **winit 在 Linux 上
  不上报**（`system_theme()` 恒为 None），egui 会落到 `fallback_theme`（默认暗色，
  浅色桌面打开就是黑的）→ `App::new` 里只在 Linux 把兜底改成亮色。别删这段
- GUI 的启动失败**必须看得见**：eframe 把错误只写进 `log::error!`
  （`eframe/src/native/run.rs`），没装 logger 就等于静默退出，双击的用户只看到
  "一闪而过"。所以 `vzip-gui` 自己装 logger 写到 exe 同目录 `vzip-gui.log`
  （`VZIP_LOG=debug|trace|off` 调级别，默认 **info**：debug 会被 wgpu / naga 灌几千行，
  所以关键结论——启动参数、后端候选、失败原因——必须我们自己在 info 上记，
  别指望默认级别下有 wgpu 的调试输出），失败再弹 `rfd` 消息框；
  release 版还带 `windows_subsystem = "windows"`，双击不再弹黑窗。别删这段
- **Windows 上默认不给 Vulkan**：有些 Intel 驱动（实测 31.0.101.2141）在
  `vkCreateDevice` 里直接崩进程，wgpu 连错误都返回不了——日志停在
  `Supported extensions:` 就断，**没有 `ERROR` 行**（别去代码里找错误处理）。
  所以 Windows 的默认候选后端是 DX12 + GL（`preferred_backends`；wgpu 只是在这两个
  里挑最好的显卡，后端崩了不会自动换，所以别再往里加可疑后端）；
  其他平台不覆盖 wgpu 默认；要强制 Vulkan 仍可 `set WGPU_BACKEND=vulkan`。别删这段
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
