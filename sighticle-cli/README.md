# sighticle-cli

Sighticle 的**无 Tauri / 无 WebView 无头 CLI**。复用 `src-tauri/src` 的截图、录屏、音频、指针、FFmpeg、分析代码，只保留 Agent 需要的命令面。

## 构建

```powershell
cargo build --release --manifest-path sighticle-cli/Cargo.toml
```

产物：`sighticle-cli/target/release/sighticle.exe`（约 1.1 MB）。

## 命令

```text
sighticle help --json
sighticle doctor
sighticle screenshot --out shot.png [--region x,y,w,h]
sighticle record --seconds 10 --out demo.mp4 [--fps 15] [--scale 1280] [--crf 28]
sighticle record --stop-file stop.flag --out demo.mp4
sighticle analyze --in demo.mp4 --frames 6 --sheet sheet.jpg
sighticle list-windows
```

- stdout 只输出一行 JSON；成功退出码 0，失败 1。
- CLI 模式不建窗口、不注册托盘/热键，可与 GUI 版并存。
- Windows 专有：WGC 屏幕采集 + WASAPI 音频。
