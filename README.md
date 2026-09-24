# VZip

把手机拍的小孩朗读打卡视频压到指定体积以内。

手机拍的朗读视频动辄几百 MB 到 1GB+，打卡平台有体积上限、平台侧转码又慢，上传经常失败。VZip 反过来算：**你告诉它上限是多少 MB，它自己决定分辨率、帧率和码率**，压完校验，超标就自动降档重压。

设计上优先保人声：音频固定 AAC 96kbps 单声道（人声足够），省下来的体积全从视频侧砍。

## 下载

[Releases](https://github.com/yoloz/VZip/releases) 里是按平台打好的包，**自带 ffmpeg，不用自己装**：

| 包 | 平台 |
|---|---|
| `vzip-linux-x64.tar.gz` | Linux x64 |
| `vzip-windows-x64.zip` | Windows x64 |
| `vzip-macos-arm64.tar.gz` | macOS（Apple 芯片） |
| `vzip-macos-x64.tar.gz` | macOS（Intel 芯片） |

解压后 `vzip` / `vzip-gui` 和 `ffmpeg` / `ffprobe` 在同一层，直接运行即可——程序优先用同目录自带的 ffmpeg，找不到才去 PATH 里翻。

包没买苹果 / 微软的签名，首次打开可能被拦：

- **macOS**：提示"无法验证开发者"时，执行 `xattr -dr com.apple.quarantine <解压目录>`
- **Windows**：SmartScreen 弹窗 →"更多信息"→"仍要运行"

Linux 的图形界面要系统有 OpenGL + xkbcommon + wayland/X11，文件选择框走 xdg-desktop-portal（GNOME / KDE 默认都有）；命令行版没这些要求。

**界面打不开怎么办**：图形界面用 wgpu 渲染，需要能用的显卡驱动（虚拟机上要开 3D 加速，否则可能起不来）。启动失败时会弹一个错误框，同时在程序同目录留下 `vzip-gui.log`，里面有详细原因；把它发出来即可。想看得更细，可以先 `set VZIP_LOG=trace`（Linux/macOS 用 `export`）再运行。

## 从源码构建

```bash
cargo build                  # 内存小于 8G 用 cargo build -j 2
```

产出两个可执行文件：`vzip`（命令行）和 `vzip-gui`（图形界面）。需要系统有 ffmpeg ≥ 4.4（含 ffprobe）。

## 发布

推 tag 即触发 GitHub Actions 出四平台包（`.github/workflows/release.yml`）：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

打包流程里会校验自带的 ffmpeg 能跑、有 libx264，并用包里的二进制真压一段做冒烟测试，坏包不会发出去。

## 命令行

```bash
vzip 输入.mp4 --max-size 20MB
```

输出同目录的 `输入_vzip.mp4`，不覆盖原片。

```
✓ 已输出：testdata/portrait_60fps_vzip.mp4
  目标 ≤20.0 MB ｜ 实际 19.4 MB ｜ 达标
  参数：1080x1920 30fps ｜ 视频 5329 kbps ｜ 音频 96 kbps 单声道 ｜ 尝试 1 次
```

压不到目标时不报错，压到质量下限（480p / 20fps）后明确告知并给建议：

```
✓ 已输出：/tmp/t4.mp4
  目标 ≤300 KB ｜ 实际 551 KB ｜ 未达标
  参数：480x854 20fps ｜ 视频 50 kbps ｜ 音频 96 kbps 单声道 ｜ 尝试 3 次
  原因：已达质量下限（480p / 20fps），继续压会看不清画面
  建议：① 剪掉片头片尾约 30 秒（可省约 20%）
        ② 拆成两段分别上传
        ③ 放宽上限（当前 300 KB，实际需要约 551 KB）
```

| 参数 | 说明 |
|---|---|
| `--max-size 20MB` | 目标体积上限，支持 KB / MB / GB 或纯字节 |
| `-o 路径` | 输出文件，默认 `原名_vzip.mp4` |
| `--audio-kbps 96` | 音频码率，默认 96 单声道 |
| `--ffmpeg 路径` | 指定 ffmpeg，默认按 PATH 找 |
| `--quiet` | 不显示进度 |

## 图形界面

```bash
cargo run --bin vzip-gui
```

拖入视频 → 填上限 → 开始。选完文件就显示"预计 720×1280 24fps · 约 18 MB"，不用等压完才知道结果。界面草稿见 `docs/ui-sketch.md`。

## 压缩策略

```
视频预算 = 目标字节 × 8 ÷ 时长 − 音频码率，再 × 0.97 安全边际
```

预算太宽时不用满（填满只会更大更慢），取 `min(预算, 原片码率×0.8)`。

选档按 `bpp = 码率 ÷ (宽 × 高 × 帧率)`：长边 1920 / 1280 / 960 / 854 与帧率 30 / 24 / 20 组合，取第一个 bpp ≥ 0.05 的档。朗读视频背景几乎不动，降帧率等于每帧多一倍码率，性价比最高。

编码用 H.264 两遍 ABR，体积命中 ±5%。详见 `docs/design.md`。

## 状态

- [x] M0 命令行
- [x] M1 图形界面 + 四平台打包（自带 ffmpeg，CI 自动出包）
- [ ] M2 引擎增强：预设档位、批量、硬件编码、取样试压、剪片头片尾

## 注意

分发包里的 ffmpeg 是 GPL 构建（静态链了 libx264 等），来源与许可见 `NOTICE.txt`。本工具以子进程方式调用 ffmpeg，比静态链接的风险低得多，但上架或商用前仍需自行评估。
