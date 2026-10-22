# 开发说明（给接手开发的人 / AI）

本文档说明项目当前状态、如何在 Windows 上验证、以及已知的待办。
最后更新：2026-10-20。

## 项目定位

给 AI Agent 用的 Windows 截图 / 录屏工具。三个设计目标：

1. **轻量** —— CLI 模式不建窗口、不注册热键、退出即释放；只有 GUI 模式常驻托盘承接快捷键。
2. **给 Agent 看** —— stdout 单行 JSON 契约，`help --json` 提供机器可读能力声明。
3. **视觉证据** —— 视频看流程与节奏，关键帧看 UI 细节，两者互补。

## 构建与运行

前置：Windows 10+、pnpm、Rust、FFmpeg（截图不需要 FFmpeg，录制/导出/抽帧需要；从源码运行时把 `ffmpeg.exe` 放到 `vendor/` 或系统 PATH，安装包则已内置）。

```powershell
pnpm install

# 开发（GUI 热更新）
pnpm tauri dev

# 单元测试（不需要前端产物，build.rs 会生成占位页）
cd src-tauri
cargo test

# 正式构建（必须先构建前端，否则 build.rs 会报错拦住）
cd ..
pnpm build
cd src-tauri
cargo build --release
```

可执行文件：`src-tauri\target\release\sightreel.exe`

冒烟测试：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\smoke-test.ps1
```

## 目录结构

| 路径 | 职责 |
| --- | --- |
| `src-tauri/src/cli.rs` | 无头 CLI：参数解析、`help --json` 能力声明、stdout 单行 JSON |
| `src-tauri/src/recorder.rs` | 录屏/截图管线：xcap WGC 抓帧 → 裁剪 → FFmpeg 编码 |
| `src-tauri/src/cursor.rs` | 指针合成：GDI 取真实指针图标 + 沿轮廓生成蓝色微光 |
| `src-tauri/src/frames.rs` | 视频分析：探测、场景检测、抽帧、拼图 |
| `src-tauri/src/ffmpeg.rs` | FFmpeg 定位器与 `doctor` 自检（安装包内置，二进制不入 git） |
| `src-tauri/src/audio.rs` | WASAPI 麦克风/系统声音采集（运行在独立线程） |
| `src-tauri/src/lib.rs` | GUI：窗口/托盘/快捷键/设置命令 |
| `src/Settings.tsx` | 设置页 |
| `src/App.tsx` / `src/RecBar.tsx` | 框选覆盖层 / 录制控制条 |

## 几个关键约束（改动前务必理解）

**1. 采集必须单线程串行**
xcap 的 D3D 设备与 WGC 采集项是进程级全局静态，并发或跨线程使用会报"参数错误"。
所有 Start/Stop/Capture 都经 `recorder.rs` 的专用工作线程转发，不要绕过。

**2. 音频必须在独立线程**
cpal/WASAPI 与 xcap/WGC 共用 COM，COM 套间状态按线程保存。混在同一线程会导致
第二次录制启动屏幕采集失败。

**3. 音频采集线程是进程级单例**
同一时刻只能有一个会话。测试里所有触及设备的用例必须持有 `audio_test_lock()`，
否则并行执行会互相抢会话（这个 bug 在真实设备上才暴露）。

**4. WGC 采集不含鼠标指针**
xcap 内部固定 `SetIsCursorCaptureEnabled(false)`，所以我们自己把指针合成进帧。
合成时 `latest` 帧必须保持干净，合成结果写到另一个缓冲（否则指针会拖影）。

**5. CLI stdout 只能有一行 JSON**
日志一律写文件。`logger::set_stdout_echo(false)` 在 CLI 启动时关闭回显。

## 当前验证状态

**已验证（Linux 环境）**
- 102 个单元测试通过（其中音频设备相关用例在无设备时会跳过）
- Windows 目标交叉编译 `cargo check --all-targets` 通过，clippy 零告警
- 时间轴/场景检测/抽帧/拼图：用真实 ffmpeg 生成测试视频做过端到端验证
- 前端 `tsc --noEmit` 与 `vite build` 通过
- 指针微光算法渲染预览确认过观感

**未验证（必须在 Windows 真机确认）**
- 指针合成的实际效果：位置是否准确、微光是否沿图标轮廓、动画指针是否正常
- WGC 录制能否反复启停
- 摄像头录制
- 字幕识别（需要用户自己的 API Key）
- 托盘、全局快捷键、开机自启

## 待办

优先级从高到低：

1. **Windows 真机验收** —— 跑 `cargo test` + 冒烟脚本 + 目视确认指针效果。
2. **`--window` 跟随窗口移动** —— 目前只在开录时定位一次，窗口拖动后画面不跟随。
   要做需要采集线程每帧重新定位并调整裁剪区域。
3. **CI** —— `.github/workflows/ci.yml` 已写好但未提交（推送 token 缺 `workflow` 权限）。
   内容包括前端类型检查/构建、Windows 测试、cargo-deny。
4. **结构化事件侧车** —— 录制时同时输出 JSON：点击坐标、时间戳、窗口切换。
   与视频互补，能显著提升 Agent 复盘精度。
5. **长录制分片** —— 目前最长 3600 秒且整段落盘。建议按 60 秒切块，支持边录边分析。

## 关于 FFmpeg

**安装包内置 FFmpeg，这是有意设计**——用户装完就能用，不该还要自己去找
FFmpeg。`tauri.conf.json` 的 `bundle.resources` 会把 `vendor/ffmpeg.exe`
写入安装包的 `resources/` 目录。

**但二进制不入 git**：`.gitignore` 忽略 `vendor/ffmpeg.exe`。两个原因：
仓库保持干净轻量；且带 x264 的 FFmpeg 是 GPLv3 构建，我们自己写的代码是
MIT，两者作为独立实体共存（用户拿到的包里有 FFmpeg 的 GPL 提供义务，
但不传染我们的源码——法律说明见 README「许可与第三方」一节）。

从源码打包前，先把 `ffmpeg.exe` 放到 `vendor/`。运行时的查找顺序
（内置副本找不到时的兜底）见 `ffmpeg.rs`：
环境变量 `SIGHTREEL_FFMPEG` → 程序同目录（含 resources/）→ PATH → 常见包管理器位置。

## 隐私边界

默认不联网。唯一例外是**可选**的字幕识别：用户填入自己的 API Key 并开启后，
导出视频的音轨会上传到所选服务商（阿里云 DashScope / 火山引擎）。
不要在该功能之外引入任何网络请求或遥测。
