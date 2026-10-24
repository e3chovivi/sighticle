<div align="center">

# Sightreel

**Let AI see what it just did — a lightweight screen recorder and capture tool**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Built with](https://img.shields.io/badge/built%20with-Tauri%202%20%2B%20Rust-24c8db)

[简体中文](README.md) | English

</div>

---

## Why Sightreel

Agents can already click buttons, run workflows and cut video for us — but they cannot see what the screen looks like after they are done. A screenshot captures one instant; a text log has no picture. Video is the closest thing to actually having watched it happen: frames, timing and rhythm all in one place.

Sightreel exposes screenshots and screen recording as commands an Agent can call directly, and ships a GUI that is pleasant for humans too. The name comes from *sight* + *reel*: eyes for the machine, a reel for the work.

## Two ways to use it

### For Agents: one command

```bash
# Start recording without a fixed duration; create stop.flag when done
sightreel record --stop-file D:/rec/stop.flag --out D:/rec/demo.mp4

# When the process exits, stdout carries exactly one line of JSON
{"ok":true,"path":"D:/rec/demo.mp4","seconds":38.2,"stopped_by":"stop_file",...}
```

Afterwards, ask the tool for a director's cut of the facts — or hand the video straight to a multimodal model:

```bash
sightreel analyze --in demo.mp4 --frames 6 --sheet sheet.jpg
# duration, resolution, frame rate, scene cuts, segments and key-frame paths in one call
```

### For Humans: tray + hotkey

The app lives in the tray and opens on a hotkey. Pick **Screenshot / Record / Camera**: drag a region, annotate, copy or save; record an area, trim it on a timeline, and export MP4 or GIF. No account, no pop-ups, no background work beyond the GUI itself.

## Features

- **Genuinely lightweight** — CLI mode opens no window and registers no hotkeys; everything is released when the process exits. Only the GUI stays in the tray to serve the global hotkey
- **Visible cursor** — screen capture never includes the pointer, so real system cursor icons are composited into the frame with a soft blue glow around their outline, making clicks and drags obvious to a model (`--no-cursor` / `--no-glow` to disable)
- **Capture and annotate** — smart window detection and region selection, with rectangles, arrows, freehand, text and mosaic, then copy or save in one action
- **Record and trim** — record a region or the full screen, export MP4 or GIF, and keep only the needed span on a timeline
- **Audio** — microphone and system sound can be enabled and gain-adjusted separately, then mixed into one track
- **Timeline for models** — `analyze` reports scene cuts and segments; `--frames` / `--sheet` also extract key frames or a contact sheet. Watch the flow as video, inspect UI details as stills — fewer tokens, better answers
- **Automation contract** — headless CLI, single-line JSON on stdout, `--stop-file` for immediate stop, `--window` to record by window title, `--fps` / `--scale` / `--crf` for size and clarity, and `help --json` for a machine-readable manifest (see [SKILL.md](SKILL.md))
- **Local first** — no account, no telemetry, no network requests by default

## Screens

<p align="center">
  <b>Smart selection: drag a region or click a window</b><br/>
  <img src="docs/images/screenshot-select.png" alt="Smart selection" width="86%" />
</p>

<p align="center">
  <b>Annotate: rectangles, arrows, freehand, text, mosaic</b><br/>
  <img src="docs/images/screenshot-annotate.png" alt="Annotate" width="86%" />
</p>

<p align="center">
  <b>Trim after recording: keep only the span you need</b><br/>
  <img src="docs/images/screenshot-editor.png" alt="Trim after recording" width="86%" />
</p>

<p align="center">
  <b>Settings: hotkey, audio gain, save location</b><br/>
  <img src="docs/images/screenshot-settings.png" alt="Settings" width="72%" />
</p>

## Install and build

- **Installer**: Windows 10 or later. Download `sightreel_<version>_x64-setup.exe` from Releases — FFmpeg is bundled, nothing else to install
- **From source**: requires [Node.js](https://nodejs.org/) + [pnpm](https://pnpm.io/) + [Rust](https://rustup.rs/)

```bash
pnpm install
pnpm tauri dev        # development
pnpm tauri build      # package (vendor/ffmpeg.exe must exist)
cd src-tauri && cargo test   # 102 unit tests
```

The installer bundles FFmpeg. The repository does not ship that binary, so place `ffmpeg.exe` in `vendor/` before building from source. At runtime, `SIGHTREEL_FFMPEG` can point to a custom binary; run `sightreel doctor` to verify. Logs can be moved to another disk with `SIGHTREEL_LOG_DIR`.

Development notes, architecture constraints and TODOs live in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## License and third parties

This project is released under the [MIT License](LICENSE).

The bundled FFmpeg is the [gyan.dev essentials build](https://www.gyan.dev/ffmpeg/builds/) (8.1.1, `--enable-gpl`) and is distributed under GPLv3. It ships as a standalone executable; its source is available at [ffmpeg.org](https://ffmpeg.org/download.html). For a fully permissive distribution, skip the bundled copy and point `SIGHTREEL_FFMPEG` at your own FFmpeg build.
