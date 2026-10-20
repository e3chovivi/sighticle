<div align="center">

# light-capture

**Lightweight Windows screen recording / capture — built so AI Agents can review their own actions**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%2010%2B-0078d4)
![Built with](https://img.shields.io/badge/built%20with-Tauri%202%20%2B%20Rust-24c8db)

[简体中文](README.md) | English

</div>

---

## The problem it solves

AI Agents are taking on more automated work: software testing, RPA workflows, video footage processing. These tasks share one gap — after the Agent finishes, there is no reliable way to verify what actually happened on screen: a screenshot captures a single moment, and text logs carry no visuals.

Video combines visuals, timing, and pacing — the most information-dense record available. light-capture exposes screen recording to AI as a single command: the Agent records its screen, retrieves the video file, and hands it to a multimodal model for review, forming a complete record → review → correct loop.

## How it works

```text
① Record    The Agent starts recording with one command, and can stop at any moment
② Collect   On exit, the process prints one line of JSON containing the video path
③ Review    The Agent passes the video to a multimodal model, then decides what to do next
```

```bash
# Start recording: no fixed duration, a "stop file" ends it anytime
light-capture record --stop-file D:/rec/stop.flag --out D:/rec/demo.mp4

# (When the Agent decides it has enough footage, it creates stop.flag — recording ends immediately)

# On exit, stdout prints exactly one line of JSON:
{"ok":true,"path":"D:/rec/demo.mp4","seconds":38.2,"stopped_by":"stop_file",...}
```

The video is an ordinary local file: no platform lock-in, no account, and it stays on your machine unless you explicitly hand it to a model — the Agent can pass it to any video-capable multimodal model.

## Key features

- **Extremely lightweight** — AI Agents are heavy consumers of hardware resources; this tool deliberately does less. In CLI mode it opens no windows, registers no hotkeys, and releases everything on exit, so idle cost is zero and only encoding resources are used while recording. Only the GUI mode keeps a tray process alive, to own the global hotkey
- **Cursor highlight** — screen capture does not include the mouse pointer by default; this tool composites the **real system cursor icon, unchanged** (arrow, I-beam, hand, resize — whatever the OS is showing), and emits a soft blue glow that follows the icon's own outline, so a reviewer can immediately see where clicks and drags happened (disable with `--no-cursor` / `--no-glow`)
- **Recording & clip selection** — area recording with MP4 / GIF export; after recording, select the segment you need on a timeline and keep only that part
- **Screenshot & annotation** — smart window detection and region selection, built-in annotation tools, copy or save as file
- **Timeline & key frames** — `analyze` returns duration, resolution, fps, scene cuts and segments, and can extract key frames or a contact sheet. Video conveys flow and pacing; stills convey UI detail — together they beat handing a model a whole recording, and cost far fewer tokens
- **Automation-friendly interface** — headless CLI, one-line JSON stdout contract, `--stop-file` for immediate abort, `--window` to record by window title, `--fps` / `--scale` / `--crf` to control size and clarity, and a machine-readable capability manifest via `help --json` (see [SKILL.md](SKILL.md))
- **Local-first** — no account, no telemetry, and no network requests by default; recordings, screenshots and logs stay on your machine (the only exception is the opt-in subtitle feature, see below)

## Typical scenarios

| Scenario | What light-capture does |
| --- | --- |
| Software testing | The Agent replays its recording to verify every click, navigation, and rendering detail |
| Automated video editing | The Agent reviews the footage first, understands pacing and content, then starts cutting |
| Workflow audit | After an automated run completes, use the video to review results and locate issues |
| Demo production | The Agent records its own actions and produces a ready-to-share walkthrough |

## Integration notes

- **Headless CLI**: no windows, no global hotkeys, zero resident processes — everything is released on exit, coexists with the GUI
- **stdout contract**: `{"ok":true,...}` on success, `{"ok":false,"error":...}` on failure, exit code 0/1; logs go to a local file only
- **Stop anytime**: recording ends the moment the `--stop-file` appears (sub-100 ms response), and the file is auto-cleaned
- **Record by window**: `light-capture list-windows` lists open windows; `record --window "<title>"` records that window directly, so the Agent never has to guess coordinates
- **Controllable size**: `--fps 15 --scale 1280 --crf 28` typically cuts the file to roughly 30–40% of the default settings, while UI detail remains readable
- **Timeline for the Agent**: `light-capture analyze --in demo.mp4 --frames 6 --sheet sheet.jpg` returns duration, resolution, scene cuts, segments and key-frame paths in one call
- **Environment check** — `light-capture doctor` reports whether FFmpeg is available, its version, the path actually resolved and where it came from; when missing it prints a copy-paste install command (this project deliberately does **not** bundle FFmpeg: screenshots don't need it, and keeping it external keeps both the installer size and the license surface clean)
- **Capability discovery**: `light-capture help --json` prints a machine-readable manifest; full integration guide in [SKILL.md](SKILL.md)
- Screenshots too: `light-capture screenshot --out shot.png [--region x,y,w,h]`
- **Configurable**: hotkey, audio gain and save directory are editable in settings; the log directory can be moved to another disk with the `LIGHT_CAPTURE_LOG_DIR` environment variable

## Humans can use it too

The same app ships a GUI: tray icon + global hotkey, smart region selection, area recording, timeline clip selection, and annotation export.

<p align="center">
  <b>Smart selection</b><br/>
  <img src="docs/images/screenshot-select.png" alt="Smart selection" width="86%" />
</p>

<p align="center">
  <b>Annotate</b><br/>
  <img src="docs/images/screenshot-annotate.png" alt="Annotate" width="86%" />
</p>

<p align="center">
  <b>Clip selection</b><br/>
  <img src="docs/images/screenshot-recbar.png" alt="Clip selection" width="86%" />
</p>

<p align="center">
  <b>Settings: hotkey, audio, cursor visualisation, subtitles, FFmpeg check</b><br/>
  <img src="docs/images/screenshot-settings.png" alt="Settings" width="72%" />
</p>

## Privacy

No account, no telemetry, and no uploads by default — recordings and screenshots never leave your machine.

The one exception is the **optional** subtitle feature: it is off by default, and only when you add your own API key and enable it does the exported audio track get uploaded to the provider you choose (Alibaba Cloud DashScope or Volcengine) for speech transcription. With it disabled, the app makes no network requests.

Logs and settings are stored locally. The settings file (API keys are stored in plain text) lives at `%LOCALAPPDATA%\io.github.e3chovivi.light-capture\settings.json`; keep it private.

## Build from source

<details>
<summary>Build steps</summary>

- Windows 10+, install [pnpm](https://pnpm.io/), [Rust](https://rustup.rs/), and [FFmpeg](https://ffmpeg.org/) (add to PATH)

```bash
pnpm install
pnpm tauri dev        # development
pnpm tauri build      # production build
cd src-tauri && cargo test   # 102 unit tests
```

</details>

## License

[MIT](LICENSE)
