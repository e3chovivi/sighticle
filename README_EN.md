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

The video is an ordinary local file: no platform lock-in, no account, no upload — the Agent can pass it to any video-capable multimodal model.

## Key features

- **Extremely lightweight** — AI Agents are heavy consumers of hardware resources; this tool deliberately does less: no resident processes, zero idle consumption, and only the necessary encoding resources while recording. Tray icon + global hotkey, ready with one keystroke
- **Recording & clip selection** — area recording with MP4 / GIF export; after recording, select the segment you need on a timeline and keep only that part
- **Screenshot & annotation** — smart window detection and region selection, built-in annotation tools, copy or save as file
- **Automation-friendly interface** — headless CLI, one-line JSON stdout contract, `--stop-file` for immediate abort, and a machine-readable capability manifest via `help --json` (see [SKILL.md](SKILL.md))
- **Local-first** — no account, no cloud upload, no telemetry; zero network requests unless recording, all data stays on your machine

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
- **Capability discovery**: `light-capture help --json` prints a machine-readable manifest; full integration guide in [SKILL.md](SKILL.md)
- Screenshots too: `light-capture screenshot --out shot.png [--region x,y,w,h]`

## Humans can use it too

The same app ships a GUI: tray icon + global hotkey, smart region selection, area recording, timeline clip selection, and annotation export.

| Smart selection | Annotate | Clip selection |
| --- | --- | --- |
| ![Smart selection](docs/images/screenshot-select.png) | ![Annotate](docs/images/screenshot-annotate.png) | ![Clip selection](docs/images/screenshot-recbar.png) |

## Privacy

Everything stays on your machine: no account, no cloud upload, no telemetry; zero network requests unless recording, and logs are written to local files only.

## Build from source

<details>
<summary>Build steps</summary>

- Windows 10+, install [pnpm](https://pnpm.io/), [Rust](https://rustup.rs/), and [FFmpeg](https://ffmpeg.org/) (add to PATH)

```bash
pnpm install
pnpm tauri dev        # development
pnpm tauri build      # production build
cd src-tauri && cargo test   # 62 unit tests
```

</details>

## License

[MIT](LICENSE)
