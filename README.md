# 终端播放一切 · TPA

跨平台纯终端视频播放器，把本地视频实时转成带颜色的 Unicode 字符，同时播放声音。画面使用普通字符和 ANSI 前景/背景颜色，不需要 Kitty、Sixel 或图形窗口。

第一版支持彩色半块 `▀`、双颜色盲文 `⣿`、ASCII 三种模式，以及暂停、跳转、音量、静音、文件浏览器、窗口适配。实际媒体支持范围取决于安装的 FFmpeg 解码器；不支持受 DRM 保护的视频。

## 安装与运行

需要 Rust 1.88+（从源码构建时）、FFmpeg 7.1+ 和 ffprobe。FFmpeg/ffprobe 应放在 PATH 中，或通过 `--ffmpeg`、`--ffprobe` 指定路径。发布包只包含播放器和文档。

- **Windows 10/11**：建议 Windows Terminal，安装 FFmpeg 后将其 `bin` 目录加入 PATH；源码构建需要 MSVC C++ 工具和 Windows SDK。
- **macOS 14.2+**：`brew install ffmpeg`，使用支持 ANSI 的终端和等宽字体。
- **Ubuntu 22.04+**：源码构建先安装 `build-essential pkg-config libasound2-dev`；另行安装 FFmpeg 7.1+。Ubuntu 22.04 默认软件源中的 FFmpeg 较旧，请确认版本。

```sh
cargo build --release --locked
cargo run --release -- doctor
cargo run --release
cargo run --release -- browse ./videos
cargo run --release -- play "视频 文件.mp4"
```

编译后的程序在 Windows 为 `target/release/tpa.exe`，其他平台为 `target/release/tpa`。可安装到 Cargo 的 bin 目录：

```sh
cargo install --path . --locked
tpa doctor
tpa play "视频 文件.mp4"
```

Linux/macOS 解压发布包后，如执行权限未保留，请运行 `chmod +x tpa`。

Windows x64 MSVC 构建静态链接 C 运行时；播放器仍使用系统的终端和音频 API。

## 命令与选项

```text
tpa                         从当前目录打开文件浏览器
tpa browse [目录]           浏览指定目录
tpa play <视频路径>         直接播放本地视频
tpa doctor                  检查工具、终端环境和默认音频设备

--mode half|braille|ascii    默认 half
--fps 1..120                Windows Terminal 默认 60，其他终端默认 30
--quality auto|fixed        默认 auto；也可固定画质
--color auto|truecolor|256|mono
--cell-aspect 0.1..2.0      字符格宽度/高度，默认 0.5
--no-audio                  使用无声播放
--ffmpeg <路径>             默认从 PATH 查找 ffmpeg
--ffprobe <路径>            默认从 PATH 查找 ffprobe
```

选项可放在子命令前或后。路径直接作为进程参数传递，包含中文、空格、`&`、`$` 等字符的文件也可使用；在 shell 中仍需要正常引用路径。

```sh
tpa play "movie.mkv" --mode braille --color truecolor
tpa play "movie.mp4" --fps 24 --no-audio
tpa play "movie.mp4" --fps 60 --quality fixed
tpa --ffmpeg "C:/Program Files/ffmpeg/bin/ffmpeg.exe" --ffprobe "C:/Program Files/ffmpeg/bin/ffprobe.exe" doctor
```

自动颜色模式识别 `COLORTERM=truecolor/24bit` 和 Windows Terminal 的 `WT_SESSION`，否则使用 256 色；`NO_COLOR` 或 `TERM=dumb` 选择单色。显式 `--color` 优先于自动判断。单色仍使用黑白前景/背景控制序列以保持字符画面可读。

`--quality auto` 会统计字符渲染、终端写入、帧调度和视频管线取帧耗时。连续 3 个一秒窗口中，超过 5% 的帧预算被错过时，先将自动真彩色降到 256 色，再把画面缩小到 90%、80%、75%，随后逐步降低帧率（最低 30 FPS；目标低于 30 时不降低目标）。连续 10 秒没有超时且平均耗时低于帧预算的 70% 后，逐级恢复画质。显式颜色设置不会被自动降色覆盖。`--quality fixed` 不会自动调整画面、颜色或帧率。

终端至少需要 40 列 × 12 行、UTF-8 和能显示 `▀`、`⣿` 的等宽字体。字符格通常宽高比约为 0.5；如果人物显得过胖或过瘦，可调整 `--cell-aspect`。160×45 字符画面是默认性能测试参考尺寸，并非分辨率保证。

## 快捷键

| 场景 | 键 | 操作 |
|---|---|---|
| 文件浏览器 | ↑ / ↓ | 选择文件或目录 |
| 文件浏览器 | Enter / Backspace | 打开 / 返回上级 |
| 文件浏览器 | `/` | 输入关键词筛选 |
| 文件浏览器 | Esc / r | 清除筛选 / 刷新 |
| 播放器 | 空格 | 暂停 / 继续 |
| 播放器 | ← / → | 后退 / 前进 5 秒 |
| 播放器 | ↑ / ↓ | 音量调整 5% |
| 播放器 | m | 静音切换 |
| 播放器 | 1 / 2 / 3 | 半块 / 盲文 / ASCII |
| 全局 | s | 打开播放设置菜单 |
| 播放器 | f | 显示 / 隐藏控制栏 |
| 播放器 | Enter | 播放结束或失败后重播 |
| 播放器 | Esc | 返回文件浏览器 |
| 全局 | ? / Esc | 打开 / 关闭帮助 |
| 全局 | q / Ctrl+C | 退出 |

筛选输入中，`q` 是普通文本；Enter 完成输入，Esc 退出输入。媒体探测期间可以按 Esc 取消。文件浏览器只列出常见视频扩展名；其他可解码文件使用 `tpa play <路径>` 打开。

在文件浏览器或播放时按 `s` 可打开设置菜单，用方向键选中帧率或画质策略。帧率可在 1–120 FPS 间按 5 FPS 调整（边界值仍可到达）；画质可在自动适配与固定画质间切换。播放中修改会立即应用，自动适配下的实际帧率和画面尺寸会显示在菜单里；这些更改只在当前运行期间生效。

窗口过小时自动暂停；恢复尺寸后继续，用户自己按下的暂停会保留。模式切换复用当前帧；窗口变化防抖 150 ms，只重建视频管线。播放结束保留最后一帧。

## 媒体与同步

ffprobe 选择默认视频、音频轨；没有默认轨则选第一条。封面图片轨不作为视频播放。

视频、音频分别通过 FFmpeg 子进程输出 RGB24 和浮点 PCM。两条管线使用相同时间原点和跳转位置，保留轨道起始偏移。视频先转为固定输出帧率，音频按时间戳补齐或修剪采样。视频旋转由 FFmpeg 自动处理，显示比例同时考虑旋转和非方形像素。

视频轨晚于音频时先显示黑帧，避免提前显示第一幅画面；片尾跳转会限制在视频的有效范围，并保留解码预滚帧。

极短片段在低帧率下仍保留一帧。未知时长的文件使用顺序解码和时间裁剪进行跳转，避免裸 H.264 等流无法进行输入定位的问题；这类文件的大幅跳转可能较慢。

声音使用 CPAL 默认设备；音频时钟根据真实消费的采样和预计设备播放延迟推进。终端跟不上时丢弃过期帧；音频缓冲不足时补零并冻结媒体时钟。无音轨、指定 `--no-audio` 或音频输出失败时改用单调时钟无声播放；音轨先于视频结束时也会平滑交接时钟。

视频队列最多 6 帧，音频环形缓冲约 500 ms，日志最多保留末尾 8 KiB。退出会取消工作线程、结束并等待子进程，恢复原始输入模式、光标和备用屏幕。断电、强制终止等无法捕获的终止不在恢复保证范围内。

首版不包含网络视频、网站解析、字幕、离线导出、播放列表、媒体库及持久化历史记录。Rust 依赖由 Cargo.lock 锁定。

## 开发与验证

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

集成测试自动合成视频，覆盖 MP4/MKV/WebM、变帧率、旋转、非方形像素、音视频偏移、非零时间原点、同步闪光和提示音、特殊字符路径、快速跳转和退出。测试不读取用户的视频，也不要求音频设备。没有 FFmpeg 时媒体集成测试会跳过；CI 设置 `TPA_REQUIRE_FFMPEG=1`，缺失工具会失败。可通过 `TPA_FFMPEG`、`TPA_FFPROBE` 指定测试工具。

真实音频设备测试需在桌面上显式执行：

```sh
cargo test --test media_integration actual_audio_device_pause_seek_and_clock -- --ignored --nocapture
```

持续播放验证复用正式播放器和渲染器，在 160×45 TestBackend 上运行；它会反复重播，测试音频设备与时钟，输出每分钟状态及结束统计。该测试衡量播放核心和字符渲染，不能替代真实终端的输出性能测试：

```sh
cargo run --release --example verify_playback -- video.mp4 1800
# 仅测试无声管线
cargo run --release --example verify_playback -- video.mp4 60 no-audio
```

Windows 可在另一个 PowerShell 中运行 `./tools/measure_memory.ps1` 记录播放器及直接子进程内存。桌面终端的人工验收步骤见 [docs/TESTING.md](docs/TESTING.md)。

Windows 还提供只使用 Python 标准库的 ConPTY 黑盒检查，直接操作正式程序的按键和窗口尺寸，并统计真彩色控制序列。CI 以 60 FPS 运行并检查 160×51 与 240×73 两档窗口。它会创建隐藏伪终端，不打开图形窗口：

```sh
python -X utf8 tools/conpty_smoke.py target/release/tpa.exe video.mp4
```

测试视频应至少 30 秒；可在末尾添加 `--no-audio` 以跳过声音。测试中的终端主机响应光标位置等查询，避免伪终端查询超时干扰结果。

CI 在 Windows、macOS、Ubuntu 上运行格式、Clippy、测试与发布构建，另有 Rust 1.88 最低版本检查。CI 产物是独立播放器 ZIP，FFmpeg 不打包。可手动打包：

```powershell
./tools/package.ps1 -Platform Windows-X64
```

MIT 许可适用于本项目源码；外部 FFmpeg 使用其自身许可。
