//! Sighticle headless CLI: screenshot / record / analyze / doctor / list-windows.
//! No Tauri, no WebView, no window, no tray.

#[macro_use]
#[path = "../../src-tauri/src/logger.rs"]
mod logger;
#[path = "../../src-tauri/src/settings.rs"]
mod settings;
#[path = "../../src-tauri/src/ffmpeg.rs"]
mod ffmpeg;
#[path = "../../src-tauri/src/frames.rs"]
mod frames;
#[path = "../../src-tauri/src/audio.rs"]
mod audio;
#[path = "../../src-tauri/src/cursor.rs"]
mod cursor;
#[path = "../../src-tauri/src/recorder.rs"]
mod recorder;
#[path = "../../src-tauri/src/rhythm.rs"]
mod rhythm;
#[path = "../../src-tauri/src/cli.rs"]
mod cli;

use std::path::{Path, PathBuf};

pub const APP_IDENTIFIER: &str = "app.sighticle.desktop";
pub const LEGACY_IDENTIFIERS: &[&str] = &[
    "io.github.echovivi.sightreel",
    "io.github.e3chovivi.light-capture",
];

pub fn log_dir(app_data: &Path) -> PathBuf {
    match std::env::var("SIGHTICLE_LOG_DIR") {
        Ok(v) if !v.trim().is_empty() => logger::prefer_dir(Path::new(v.trim()), app_data),
        _ => app_data.to_path_buf(),
    }
}

pub fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

#[derive(Debug, serde::Serialize)]
pub struct ScreenshotData {
    pub width: u32,
    pub height: u32,
    pub data_url: String,
}

pub fn decode_data_url(data_url: &str) -> Result<Vec<u8>, String> {
    let b64 = data_url
        .split_once(",")
        .map(|(_, rest)| rest)
        .ok_or_else(|| "无效的图片数据".to_string())?;
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cli::run(&args));
}
