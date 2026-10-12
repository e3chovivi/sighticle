//! light-capture：轻量截图 / 录屏工具
//! M1：托盘常驻 + 全局快捷键唤起模式选择层 + 区域框选上报
//! M2/M3：截图（xcap）+ 标注后复制/保存；区域录屏（xcap + FFmpeg）

pub mod cli;

mod asr;
mod audio;
mod logger;
mod recorder;
mod settings;

use std::path::Path;

use base64::Engine;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// 日志根目录：放 D 盘，不占用系统盘。若该盘不可用（换台只有 C 盘的机器）自动回退。
const LOG_ROOT_ON_D: &str = r"D:\light-capture";

const OVERLAY_LABEL: &str = "overlay";
const RECBAR_LABEL: &str = "recbar";
const SETTINGS_LABEL: &str = "settings";

/// 屏幕上的一个点（逻辑像素，以窗口左上角为原点）
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// 框选区域。允许任意方向拖拽，统一归一化为正宽高
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Selection {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Selection {
    pub fn from_points(a: Point, b: Point) -> Self {
        Self {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            width: (a.x - b.x).abs(),
            height: (a.y - b.y).abs(),
        }
    }

    /// 过滤误触：小于 2 逻辑像素的选区视为无效
    pub fn is_valid(&self) -> bool {
        self.width >= 2.0 && self.height >= 2.0
    }
}

/// 规范化用户输入的快捷键为 global-hotkey 解析格式（如 "Ctrl+Shift+Q" -> "ctrl+shift+q"）。
/// 修饰键按固定顺序输出，保证同一组合的不同写法得到同一结果（M5 自定义快捷键复用）。
pub fn normalize_hotkey(input: &str) -> Result<String, String> {
    // flags: ctrl / shift / alt / super
    let mut flags = [false; 4];
    let mut key: Option<String> = None;
    for part in input.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => flags[0] = true,
            "shift" => flags[1] = true,
            "alt" => flags[2] = true,
            "meta" | "win" | "super" | "cmd" | "command" => flags[3] = true,
            k if key.is_none() => key = Some(k.to_string()),
            _ => return Err(format!("无效的快捷键: {input}")),
        }
    }
    let key = key.ok_or_else(|| format!("快捷键缺少主键: {input}"))?;
    let mut out = String::new();
    for (on, name) in flags.iter().zip(["ctrl", "shift", "alt", "super"]) {
        if *on {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(&key);
    Ok(out)
}

/// 时间戳：20260913_183005，用于文件名
pub fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

fn toggle_overlay(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(OVERLAY_LABEL) {
        match win.is_visible() {
            Ok(true) => {
                lclog!("覆盖层：隐藏");
                let _ = win.hide();
            }
            _ => {
                lclog!("覆盖层：显示");
                let _ = win.show();
                let _ = win.set_focus();
            }
        }
    }
}

/// 截图数据：物理像素宽高 + PNG data URL（前端 Canvas 直接绘制）
#[derive(Debug, serde::Serialize)]
pub struct ScreenshotData {
    pub width: u32,
    pub height: u32,
    pub data_url: String,
}

/// 解析 data:image/png;base64,xxx 为 PNG 字节
fn decode_data_url(data_url: &str) -> Result<Vec<u8>, String> {
    let b64 = data_url
        .split_once(",")
        .map(|(_, rest)| rest)
        .ok_or_else(|| "无效的图片数据".to_string())?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn hide_overlay(app: AppHandle) {
    if let Some(win) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = win.hide();
    }
}

/// 把录制控制条小窗口摆放到选区下方（贴近底部时翻转到上方），并显示。
/// 入参为覆盖层逻辑坐标；控制条是独立窗口，录制时全屏覆盖层已隐藏，鼠标不受阻。
#[tauri::command]
fn position_recbar(app: AppHandle, region: Selection) -> Result<(), String> {
    let (px, py, pw, ph) = overlay_physical_region(&app, &region)?;
    let bar = app
        .get_webview_window(RECBAR_LABEL)
        .ok_or_else(|| "控制条窗口不存在".to_string())?;
    // 控制条物理高度（按窗口自身缩放换算）
    let scale = bar.scale_factor().map_err(|e| e.to_string())?;
    let bar_h = (60.0 * scale) as i32;
    // 屏幕物理高度（用覆盖层窗口所在屏）
    let overlay = app
        .get_webview_window(OVERLAY_LABEL)
        .ok_or_else(|| "覆盖层窗口不存在".to_string())?;
    let ph = ph as i32;
    let pw = pw as i32;
    let mon = overlay.current_monitor().map_err(|e| e.to_string())?;
    let screen_bottom = mon
        .map(|m| m.position().y + m.size().height as i32)
        .unwrap_or(py + ph);
    let below = py + ph + 12;
    let top = if below + bar_h > screen_bottom - 8 {
        (py - bar_h - 12).max(8)
    } else {
        below
    };
    let left = px.max(8).min((px + pw).saturating_sub(8));
    bar.set_position(tauri::PhysicalPosition::new(left, top))
        .map_err(|e| e.to_string())?;
    bar.show().map_err(|e| e.to_string())?;
    bar.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

/// 摄像头录制没有选区：控制条放到屏幕底部居中（等价于"选了整屏"时的观感）
fn position_recbar_center_bottom(app: AppHandle) -> Result<(), String> {
    let bar = app
        .get_webview_window(RECBAR_LABEL)
        .ok_or_else(|| "控制条窗口不存在".to_string())?;
    let scale = bar.scale_factor().map_err(|e| e.to_string())?;
    let bar_w = (380.0 * scale) as i32;
    let bar_h = (60.0 * scale) as i32;
    let monitor = match bar.current_monitor().map_err(|e| e.to_string())? {
        Some(m) => m,
        None => return Err("获取显示器失败".into()),
    };
    let mx = monitor.position().x;
    let my = monitor.position().y;
    let mw = monitor.size().width as i32;
    let mh = monitor.size().height as i32;
    let left = mx + (mw - bar_w) / 2;
    let top = my + mh - bar_h - 24;
    bar.set_position(tauri::PhysicalPosition::new(left, top))
        .map_err(|e| e.to_string())?;
    bar.show().map_err(|e| e.to_string())?;
    bar.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

/// 切换控制条窗口的布局：recording（380×60）/ editor（660×460）/ done（470×210），
/// 并保证放大后不越出屏幕。
///
/// 关键说明：窗口几何操作必须放在 Rust 侧——`core:window:default` 只包含只读权限
/// （读位置/尺寸/可见性），不含 set-size / set-position / hide。前端 JS 调用这些
/// 接口会被权限系统拒绝且异常被吞掉（静默失败），曾导致停止录制后窗口没放大、
/// 编辑器内容溢出到窗口外、保存按钮完全点不到。
#[tauri::command]
fn layout_recbar(app: AppHandle, stage: String) -> Result<(), String> {
    let win = app
        .get_webview_window(RECBAR_LABEL)
        .ok_or_else(|| "控制条窗口不存在".to_string())?;
    let (w, h) = match stage.as_str() {
        "editor" => (660.0_f64, 460.0),
        "done" => (470.0, 210.0),
        _ => (380.0, 60.0),
    };
    let scale = win.scale_factor().map_err(|e| e.to_string())?;
    let pw = (w * scale).round() as i32;
    let ph = (h * scale).round() as i32;
    // 放大后保持窗口留在屏幕内：优先保持当前位置，越界则回收到边界
    if let Ok(Some(mon)) = win.current_monitor() {
        let mpos = mon.position();
        let msize = mon.size();
        if let Ok(cur) = win.outer_position() {
            let min_x = mpos.x + 8;
            let min_y = mpos.y + 8;
            let max_x = (mpos.x + msize.width as i32 - pw - 8).max(min_x);
            let max_y = (mpos.y + msize.height as i32 - ph - 8).max(min_y);
            let nx = cur.x.clamp(min_x, max_x);
            let ny = cur.y.clamp(min_y, max_y);
            win.set_position(tauri::PhysicalPosition::new(nx, ny))
                .map_err(|e| e.to_string())?;
        }
    }
    win.set_size(tauri::LogicalSize::new(w, h))
        .map_err(|e| e.to_string())?;
    win.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

/// 隐藏控制条窗口（放弃 / 完成时使用）
#[tauri::command]
fn hide_recbar(app: AppHandle) {
    if let Some(win) = app.get_webview_window(RECBAR_LABEL) {
        let _ = win.hide();
    }
}

/// 显示（并聚焦）覆盖层窗口。
/// 截图/录屏前会把覆盖层临时藏起来（见下方说明），完成后由前端调它恢复。
#[tauri::command]
fn show_overlay(app: AppHandle) {
    if let Some(win) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// 隐藏覆盖层并等待合成器真正把它从画面里移除。
///
/// 为什么必须这么做：启用 xcap 的 WGC 后，采集到的是**真实合成画面**，
/// 会把我们自己覆盖层的内容一起拍进去——覆盖层带一层深色蒙版，且框选区域上还叠了
/// 10% 的紫色填充，结果就是截图/录像整体"蒙了一层紫色"。
/// `hide()` 是异步生效的，必须留出一点时间让 DWM 重新合成，否则仍可能拍到残影。
fn hide_overlay_and_settle(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = win.hide();
    }
    std::thread::sleep(OVERLAY_HIDE_SETTLE);
}

/// 覆盖层隐藏后等待合成的时长。取两帧左右，兼顾可靠性与响应速度。
const OVERLAY_HIDE_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

/// 覆盖层窗口内的逻辑选区 → 全局物理像素区域。
/// 覆盖层是全屏窗口，其窗口坐标 + 自身缩放因子即可把 client 坐标精确换算到物理像素，
/// 天然兼容多显示器与混合缩放，无需自行推导多屏布局。
fn overlay_physical_region(app: &AppHandle, region: &Selection) -> Result<(i32, i32, u32, u32), String> {
    let win = app
        .get_webview_window(OVERLAY_LABEL)
        .ok_or_else(|| "覆盖层窗口不存在".to_string())?;
    let pos = win.outer_position().map_err(|e| e.to_string())?;
    let scale = win.scale_factor().map_err(|e| e.to_string())?;
    let px = pos.x + (region.x * scale).round() as i32;
    let py = pos.y + (region.y * scale).round() as i32;
    let pw = (region.width * scale).round() as u32;
    let ph = (region.height * scale).round() as u32;
    Ok((px, py, pw, ph))
}

/// 截图：经采集工作线程按选区抓取（物理像素），返回 PNG data URL 供前端标注。
/// 截图与录屏共用同一条工作线程串行执行（见 recorder.rs 模块头说明），
/// 保证 xcap 内部全局 D3D 状态不被并发使用，根治"参数错误"。
///
/// 注意：采集前会先把覆盖层隐藏（见 hide_overlay_and_settle），因此返回后
/// 前端必须调用 show_overlay 把界面显示出来。
#[tauri::command]
async fn capture_region_screenshot(app: AppHandle, region: Selection) -> Result<ScreenshotData, String> {
    if !region.is_valid() {
        crate::lclog!("截图请求被拒：选区过小");
        return Err("选区过小".into());
    }
    // 物理区域依赖覆盖层窗口的位置，必须在隐藏之前算好
    let (px, py, pw, ph) = overlay_physical_region(&app, &region)?;
    crate::lclog!(
        "截图请求：逻辑=({},{} {}x{}) 物理=({px},{py} {pw}x{ph})",
        region.x,
        region.y,
        region.width,
        region.height
    );
    let app2 = app.clone();
    let shot = tauri::async_runtime::spawn_blocking(move || {
        hide_overlay_and_settle(&app2);
        recorder::capture(px, py, pw, ph)
    })
    .await
    .map_err(|e| format!("截图任务异常：{e}"))?;
    if let Err(e) = &shot {
        crate::lclog!("!! 截图失败：{e}");
        // 失败也要把界面还回来，否则用户面对一个"消失的界面"
        if let Some(win) = app.get_webview_window(OVERLAY_LABEL) {
            let _ = win.show();
        }
    }
    shot
}

/// 把标注完成的图片（PNG data URL）复制到系统剪贴板
#[tauri::command]
fn copy_image_to_clipboard(data_url: String) -> Result<(), String> {
    let png = decode_data_url(&data_url)?;
    let img = image::load_from_memory(&png).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard
        .set_image(arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: std::borrow::Cow::Owned(rgba.into_raw()),
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 保存标注完成的图片到用户通过「另存为」对话框选择的路径
#[tauri::command]
fn save_image(data_url: String, path: String) -> Result<String, String> {
    let png = decode_data_url(&data_url)?;
    std::fs::write(&path, &png).map_err(|e| e.to_string())?;
    Ok(path)
}

/// 覆盖层内的窗口矩形（逻辑坐标），用于飞书式悬停识别 / 单击选窗
#[derive(Debug, serde::Serialize)]
pub struct WindowRect {
    pub title: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 枚举当前可见窗口，换算为覆盖层逻辑坐标。
/// 跳过最小化、无标题以及本应用自身的窗口。
#[tauri::command]
fn list_windows(app: AppHandle) -> Result<Vec<WindowRect>, String> {
    let overlay = app
        .get_webview_window(OVERLAY_LABEL)
        .ok_or_else(|| "覆盖层窗口不存在".to_string())?;
    let pos = overlay.outer_position().map_err(|e| e.to_string())?;
    let scale = overlay.scale_factor().map_err(|e| e.to_string())?;
    let windows = xcap::Window::all().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for w in windows {
        if w.is_minimized().unwrap_or(true) {
            continue;
        }
        let title = w.title().unwrap_or_default();
        if title.trim().is_empty() || title == "light-capture" {
            continue;
        }
        let (wx, wy) = (w.x().unwrap_or(0), w.y().unwrap_or(0));
        let (ww, wh) = (w.width().unwrap_or(0), w.height().unwrap_or(0));
        if ww < 8 || wh < 8 {
            continue;
        }
        out.push(WindowRect {
            title,
            x: (wx - pos.x) as f64 / scale,
            y: (wy - pos.y) as f64 / scale,
            width: ww as f64 / scale,
            height: wh as f64 / scale,
        });
    }
    Ok(out)
}

/// 录制开始时发给控制条的事件载荷：携带实际启用成功的音频轨道，
/// 让控制条能如实展示"正在录什么声音"。
#[derive(Clone, serde::Serialize)]
struct RecStarted {
    mic: bool,
    sys_audio: bool,
    audio_warnings: Vec<String>,
}

/// 开始录屏（选区为逻辑坐标）。
/// 成功后隐藏全屏覆盖层、显示独立控制条窗口——录制期间鼠标操作完全不受阻。
/// mic / sys_audio：是否采集麦克风与系统声音（均无需安装虚拟声卡）。
/// async 命令在独立线程池执行，避免阻塞主线程导致界面卡死。
#[tauri::command]
async fn start_recording(
    app: AppHandle,
    mode: String,
    region: Selection,
    mic: bool,
    sys_audio: bool,
) -> Result<recorder::StartInfo, String> {
    crate::lclog!(
        "收到录屏请求：mode={mode} 逻辑区域=({},{},{}x{}) mic={mic} sys={sys_audio}",
        region.x,
        region.y,
        region.width,
        region.height
    );
    // 摄像头录制没有选区概念：跳过区域校验与坐标换算，控制条放屏幕底部居中
    let camera = mode == "camera";
    if !camera && !region.is_valid() {
        crate::lclog!("录屏请求被拒：选区过小");
        return Err("选区过小".into());
    }
    let (px, py, pw, ph) = if camera {
        (0, 0, 0, 0)
    } else {
        overlay_physical_region(&app, &region)?
    };
    // 录制临时文件目录（recorder 已不感知 Tauri，由调用方给出）
    let rec_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("recordings");
    // 先藏起覆盖层再开始采集：WGC 采集真实合成画面，否则录像开头会拍进
    // 覆盖层的深色蒙版与紫色选区（与截图偏紫是同一个原因）
    let app2 = app.clone();
    let started = tauri::async_runtime::spawn_blocking(move || {
        hide_overlay_and_settle(&app2);
        recorder::start(rec_dir, mode, px, py, pw, ph, mic, sys_audio)
    })
    .await
    .map_err(|e| format!("录制任务异常：{e}"))?;
    let info = match started {
        Ok(i) => i,
        Err(e) => {
            // 启动失败：把覆盖层还回来，用户才能重新选区域重试
            if let Some(win) = app.get_webview_window(OVERLAY_LABEL) {
                let _ = win.show();
            }
            return Err(e);
        }
    };
    let _ = if camera {
        position_recbar_center_bottom(app.clone())
    } else {
        position_recbar(app.clone(), region)
    };
    // 通知控制条窗口重置状态（计时清零、回到录制态），并告知实际启用的音频轨道
    let _ = app.emit_to(
        RECBAR_LABEL,
        "rec-started",
        RecStarted {
            mic: info.mic,
            sys_audio: info.sys_audio,
            audio_warnings: info.audio_warnings.clone(),
        },
    );
    Ok(info)
}

/// 停止录屏：discard=true 删除临时文件；否则返回临时文件路径，
/// 由前端编辑器（RecBar）选段后导出到用户选择的位置。
#[tauri::command]
async fn stop_recording(discard: bool) -> Result<recorder::StopResult, String> {
    recorder::stop(discard)
}

/// 导出录屏：把临时录制文件按 [start, end] 裁剪编码为 MP4 或 GIF，写入用户选择的目标路径。
/// spawn_blocking：FFmpeg 裁剪编码可能耗时较长，不占用异步运行时线程。
#[tauri::command]
async fn export_recording(src: String, start: f64, end: f64, dest: String, format: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || recorder::export(&src, start, end, &dest, &format))
        .await
        .map_err(|e| e.to_string())?
}

/// 放弃录制：删除临时文件
#[tauri::command]
fn discard_recording(src: String) -> Result<(), String> {
    recorder::discard(&src)
}

// ================= 设置（快捷键 / 音频增益 / 开机自启） =================

/// 前端展示用的设置视图
#[derive(Debug, serde::Serialize)]
struct SettingsView {
    /// 规范化键位（如 "ctrl+shift+q"）
    hotkey: String,
    /// 便于阅读的键位（如 "Ctrl + Shift + Q"）
    hotkey_display: String,
    mic_gain: f32,
    sys_gain: f32,
    autostart: bool,
    /// 截图默认保存目录：空 = 每次询问保存位置
    save_dir: String,
    log_path: String,
    mic_gain_range: (f32, f32),
    sys_gain_range: (f32, f32),
    kill_hotkey_display: String,
    /// 字幕识别配置（含凭证与可用性判定，设置页用）
    subtitle: SubtitleView,
}

/// 字幕识别配置的前端视图
#[derive(Debug, Clone, serde::Serialize)]
struct SubtitleView {
    enabled: bool,
    /// "qwen" | "doubao"
    provider: String,
    qwen_api_key: String,
    doubao_app_key: String,
    doubao_access_token: String,
    /// 当前服务商凭证是否齐全（决定开关能否打开）
    creds_ready: bool,
}

/// 把规范化键位转成便于阅读的形式
fn hotkey_display(hk: &str) -> String {
    hk.split('+')
        .map(|p| match p.trim() {
            "ctrl" => "Ctrl".to_string(),
            "shift" => "Shift".to_string(),
            "alt" => "Alt".to_string(),
            "super" => "Win".to_string(),
            other => other.to_uppercase(),
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

fn settings_view() -> SettingsView {
    let s = settings::get();
    SettingsView {
        hotkey_display: hotkey_display(&s.hotkey),
        hotkey: s.hotkey,
        mic_gain: s.mic_gain,
        sys_gain: s.sys_gain,
        autostart: s.autostart,
        save_dir: s.save_dir.clone().unwrap_or_default(),
        log_path: logger::path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        mic_gain_range: settings::MIC_GAIN_RANGE,
        sys_gain_range: settings::SYS_GAIN_RANGE,
        kill_hotkey_display: hotkey_display(settings::KILL_HOTKEY),
        subtitle: SubtitleView {
            enabled: s.subtitle.enabled,
            provider: s.subtitle.provider.clone(),
            qwen_api_key: s.subtitle.qwen_api_key.clone(),
            doubao_app_key: s.subtitle.doubao_app_key.clone(),
            doubao_access_token: s.subtitle.doubao_access_token.clone(),
            creds_ready: s.subtitle.creds_ready(),
        },
    }
}

/// 注册唤起键位。占用冲突会返回错误——这就是系统级的"冲突检测"：
/// 由 Windows 亲自告诉我们该组合是否已被别的程序抢走。
fn register_toggle_hotkey(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    let sc: Shortcut = hotkey
        .parse()
        .map_err(|_| format!("无法识别的键位：{hotkey}"))?;
    app.global_shortcut()
        .on_shortcut(sc, |app, _sc, event| {
            if event.state == ShortcutState::Pressed {
                toggle_overlay(app);
            }
        })
        .map_err(|e| format!("该键位不可用（可能已被其它程序占用）：{e}"))
}

#[tauri::command]
fn get_settings() -> SettingsView {
    settings_view()
}

/// 修改全局唤起键位：规范化 → 与强杀键位比对 → 解除旧键位 → 试注册新键位。
/// 注册失败会把旧键位恢复回来，保证应用不会"失去唤起能力"。
#[tauri::command]
fn set_hotkey(app: AppHandle, hotkey: String) -> Result<SettingsView, String> {
    let normalized = normalize_hotkey(&hotkey)?;
    let kill = normalize_hotkey(settings::KILL_HOTKEY)?;
    if normalized == kill {
        return Err("这个键位已留给「强制退出」，请换一个".into());
    }
    let old = settings::get().hotkey;
    if normalized == old {
        return Ok(settings_view());
    }
    if let Ok(sc) = old.parse::<Shortcut>() {
        let _ = app.global_shortcut().unregister(sc);
    }
    match register_toggle_hotkey(&app, &normalized) {
        Ok(()) => {
            let _ = settings::update(|s| {
                s.hotkey = normalized.clone();
                Ok(())
            });
            crate::lclog!("全局快捷键已更新为 {normalized}");
            Ok(settings_view())
        }
        Err(e) => {
            crate::lclog!("!! 注册键位 {normalized} 失败：{e}（回退到 {old}）");
            if let Err(restore) = register_toggle_hotkey(&app, &old) {
                crate::lclog!("!! 回退旧键位 {old} 也失败了：{restore}");
            }
            Err(e)
        }
    }
}

#[tauri::command]
fn set_audio_gains(mic_gain: f32, sys_gain: f32) -> Result<SettingsView, String> {
    settings::update(|s| {
        s.mic_gain = mic_gain;
        s.sys_gain = sys_gain;
        Ok(())
    })?;
    crate::lclog!("音频增益已更新：mic={mic_gain} sys={sys_gain}");
    Ok(settings_view())
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<SettingsView, String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let res = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    res.map_err(|e| format!("修改开机自启失败：{e}"))?;
    settings::update(|s| {
        s.autostart = enabled;
        Ok(())
    })?;
    crate::lclog!("开机自启已设为 {enabled}");
    Ok(settings_view())
}

/// 设置截图默认保存目录。传空字符串 = 恢复「每次询问保存位置」。
#[tauri::command]
fn set_save_dir(dir: String) -> Result<SettingsView, String> {
    settings::update(|s| {
        s.save_dir = if dir.trim().is_empty() {
            None
        } else {
            Some(dir.trim().to_string())
        };
        Ok(())
    })?;
    crate::lclog!(
        "截图保存目录已更新：{}",
        if dir.trim().is_empty() {
            "每次询问"
        } else {
            dir.trim()
        }
    );
    Ok(settings_view())
}

/// 保存字幕识别配置。凭证不齐时 sanitize 会强制 enabled=false（没填 = 功能不存在）。
#[tauri::command]
fn set_subtitle_config(
    enabled: bool,
    provider: String,
    qwen_api_key: String,
    doubao_app_key: String,
    doubao_access_token: String,
) -> Result<SettingsView, String> {
    settings::update(|s| {
        s.subtitle.enabled = enabled;
        s.subtitle.provider = provider;
        s.subtitle.qwen_api_key = qwen_api_key;
        s.subtitle.doubao_app_key = doubao_app_key;
        s.subtitle.doubao_access_token = doubao_access_token;
        Ok(())
    })?;
    let v = settings_view();
    crate::lclog!(
        "字幕配置已保存：provider={} enabled={} 凭证齐全={}",
        v.subtitle.provider,
        v.subtitle.enabled,
        v.subtitle.creds_ready
    );
    Ok(v)
}

/// 为导出的录屏生成字幕：识别 → 同名 .srt → 内嵌 MP4 软字幕轨。
/// 返回 `{ srt, embedded }`；`srt` 为空串 = 字幕未启用（前端静默跳过）。
/// spawn_blocking：网络请求 + 轮询可能耗时数十秒，不占用异步运行时。
#[tauri::command]
async fn generate_subtitle(video: String) -> Result<asr::SubtitleOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || asr::generate_subtitle(&video))
        .await
        .map_err(|e| format!("字幕任务异常：{e}"))?
}

/// 测试字幕服务商连通性（验证网络 + 凭证；Qwen 走免费鉴权接口，豆包发 1 秒静音）。
#[tauri::command]
async fn test_subtitle_connection() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(asr::test_connection)
        .await
        .map_err(|e| format!("测试任务异常：{e}"))?
}

/// 强杀保险丝：**不依赖主线程消息循环**的兜底退出通道。
///
/// 为什么光有全局快捷键不够：全局快捷键走 WM_HOTKEY，必须由主线程的消息循环处理。
/// 一旦主线程被卡住（例如在同步命令里创建 WebView 触发 wry#583 自锁），强杀键就形同虚设 ——
/// 而全屏置顶的覆盖层还盖着整个桌面，用户连任务管理器都露不出来，只能重启电脑。
/// 这里改用独立线程轮询键盘物理状态（GetAsyncKeyState），命中即直接结束进程，
/// 无论 UI 线程处于什么状态都拦不住它。
#[cfg(windows)]
fn spawn_kill_watchdog() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_Q, VK_SHIFT,
    };
    let spawned = std::thread::Builder::new()
        .name("lc-kill-watch".into())
        .spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
            // 返回值的最高位为 1 表示"此刻物理按下"
            let down = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) < 0 };
            if down(VK_CONTROL) && down(VK_SHIFT) && down(VK_MENU) && down(VK_Q) {
                lclog!("触发强杀键 {}，立即结束进程", settings::KILL_HOTKEY);
                std::process::exit(0);
            }
        });
    match spawned {
        Ok(_) => lclog!(
            "强杀看门狗已启动（{}，独立线程轮询）",
            settings::KILL_HOTKEY
        ),
        Err(e) => lclog!("!! 强杀看门狗线程启动失败：{e}"),
    }
}

/// 真正创建设置窗口。调用方必须保证它**不在主线程**上执行（原因见 open_settings）。
fn build_settings_window(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(
        app,
        SETTINGS_LABEL,
        WebviewUrl::App("index.html#settings".into()),
    )
    .title("light-capture 设置")
    .inner_size(560.0, 560.0)
    .resizable(false)
    .center()
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 打开设置窗口（按需创建，不常驻，符合"轻量化"目标）。
///
/// **必须是 async**：同步命令在主线程的事件循环里执行，而这里要新建 WebView。
/// 在已有 WebView 的 IPC 回调里再建一个 WebView 会自锁（wry#583，Tauri 官方文档
/// 明确列为 Windows 已知问题）—— 主线程一卡，托盘、IPC、全局快捷键全部失效。
/// async 命令由异步运行时的线程执行，窗口创建会被正确地派发回主线程。
#[tauri::command]
async fn open_settings(app: AppHandle) -> Result<(), String> {
    lclog!("命令 open_settings：创建设置窗口");
    match build_settings_window(&app) {
        Ok(()) => {
            lclog!("设置窗口已就绪");
            Ok(())
        }
        Err(e) => {
            lclog!("!! 创建设置窗口失败：{e}");
            Err(e)
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            hide_overlay,
            show_overlay,
            position_recbar,
            layout_recbar,
            hide_recbar,
            get_settings,
            set_hotkey,
            set_audio_gains,
            set_autostart,
            set_save_dir,
            set_subtitle_config,
            generate_subtitle,
            test_subtitle_connection,
            open_settings,
            capture_region_screenshot,
            copy_image_to_clipboard,
            save_image,
            list_windows,
            start_recording,
            stop_recording,
            export_recording,
            discard_recording
        ])
        .setup(|app| {
            // 日志尽早初始化：后续每一步关键流程与**原始错误详情**都会落到文件，
            // 用户复现一次即可事后精确定位（GUI 里只能看到笼统提示）。
            if let Ok(dir) = app.path().app_local_data_dir() {
                // 日志放 D 盘（用户要求，且日志是排查依据不占系统盘）；设置仍留在系统数据目录，
                // 换目录会让用户已配置的快捷键/增益凭空丢失。
                logger::init(&logger::prefer_dir(Path::new(LOG_ROOT_ON_D), &dir));
                settings::init(&dir);
            }
            lclog!(
                "应用启动完成；日志路径={:?}；标识符={}",
                logger::path(),
                app.config().identifier
            );

            // 覆盖层窗口：全屏透明、无边框、置顶、不占任务栏，默认隐藏
            WebviewWindowBuilder::new(app, OVERLAY_LABEL, WebviewUrl::App("index.html".into()))
                .title("light-capture")
                .fullscreen(true)
                .transparent(true)
                .decorations(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .visible(false)
                .build()?;

            // 录制控制条窗口：独立小窗口，录制时全屏覆盖层隐藏，鼠标操作完全不受阻
            WebviewWindowBuilder::new(
                app,
                RECBAR_LABEL,
                WebviewUrl::App("index.html#recbar".into()),
            )
            .title("light-capture-recbar")
            .inner_size(380.0, 60.0)
            .transparent(true)
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .visible(false)
            .build()?;

            // 清理上次运行遗留的录制临时文件（启动时不可能有进行中的录制）
            if let Ok(dir) = app.path().app_local_data_dir().map(|d| d.join("recordings")) {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        // 清理上次异常退出遗留的临时视频与音频文件
                        let stale = (name.starts_with("tmp_") && name.ends_with(".mp4"))
                            || (name.ends_with(".wav")
                                && (name.starts_with("mic") || name.starts_with("sys")));
                        if stale {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }

            // 系统托盘
            let capture = MenuItem::with_id(
                app,
                "capture",
                format!("截图 / 录屏 ({})", hotkey_display(&settings::get().hotkey)),
                true,
                None::<&str>,
            )?;
            let settings_item = MenuItem::with_id(app, "settings", "设置…", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&capture, &settings_item, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().expect("缺少应用图标").clone())
                .menu(&menu)
                .tooltip("light-capture")
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "capture" => toggle_overlay(app),
                    "settings" => {
                        // 托盘回调同样运行在主线程：这里也不能直接建 WebView，
                        // 交给异步运行时（窗口创建会被正确地派发回主线程）
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = build_settings_window(&app) {
                                lclog!("!! 打开设置窗口失败：{e}");
                            }
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_overlay(tray.app_handle());
                    }
                })
                .build(app)?;

            // 全局快捷键：键位来自用户设置（设置页可改，含系统级冲突检测）
            let hk = settings::get().hotkey;
            match register_toggle_hotkey(app.handle(), &hk) {
                Ok(()) => lclog!("全局快捷键已注册：{hk}"),
                // 最常见原因是**已有实例在运行**并占用了同一热键。
                // 不致命：日志留痕，托盘仍可用，避免抛 panic 糊用户一脸。
                Err(e) => lclog!("!! 注册全局快捷键 {hk} 失败（可能已有实例在运行）：{e}"),
            }

            // 强杀保险丝（两条通道，互为备份）：
            //  1) 全局快捷键：正常时最快，但依赖主线程消息循环；
            //  2) 独立线程轮询看门狗：主线程卡死时唯一还能生效的逃生通道。
            let kill: Shortcut = settings::KILL_HOTKEY.parse().expect("强杀快捷键无效");
            if let Err(e) = app.global_shortcut().on_shortcut(kill, |_app, _sc, event| {
                if event.state == ShortcutState::Pressed {
                    // 不用 app.exit()：它只是往事件循环投递退出请求，
                    // 主线程一旦卡住就永远处理不到。直接终结进程，不给卡死留机会。
                    lclog!("触发强杀键 {}，立即结束进程", settings::KILL_HOTKEY);
                    std::process::exit(0);
                }
            }) {
                lclog!(
                    "!! 注册强杀快捷键 {} 失败：{e}",
                    settings::KILL_HOTKEY
                );
            }
            #[cfg(windows)]
            spawn_kill_watchdog();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    #[test]
    fn normalizes_forward_drag() {
        let s = Selection::from_points(p(10.0, 20.0), p(70.0, 90.0));
        assert_eq!(
            s,
            Selection {
                x: 10.0,
                y: 20.0,
                width: 60.0,
                height: 70.0
            }
        );
    }

    #[test]
    fn normalizes_reverse_drag() {
        let s = Selection::from_points(p(100.0, 100.0), p(40.0, 30.0));
        assert_eq!(
            s,
            Selection {
                x: 40.0,
                y: 30.0,
                width: 60.0,
                height: 70.0
            }
        );
    }

    #[test]
    fn rejects_tiny_selection() {
        assert!(!Selection::from_points(p(5.0, 5.0), p(6.5, 6.5)).is_valid());
        assert!(Selection::from_points(p(0.0, 0.0), p(8.0, 8.0)).is_valid());
    }

    #[test]
    fn normalizes_hotkey_variants() {
        assert_eq!(normalize_hotkey("Ctrl+Shift+Q").unwrap(), "ctrl+shift+q");
        assert_eq!(normalize_hotkey("SHIFT + CTRL + q").unwrap(), "ctrl+shift+q");
        assert_eq!(normalize_hotkey("Control+Alt+F7").unwrap(), "ctrl+alt+f7");
        assert_eq!(normalize_hotkey("Alt+P").unwrap(), "alt+p");
        assert!(normalize_hotkey("Ctrl+").is_err());
        assert!(normalize_hotkey("Ctrl+A+B").is_err());
    }

    #[test]
    fn default_hotkey_is_parseable() {
        assert!(settings::DEFAULT_HOTKEY.parse::<Shortcut>().is_ok());
        assert!(settings::KILL_HOTKEY.parse::<Shortcut>().is_ok());
    }

    #[test]
    fn decodes_data_url() {
        // 1x1 红色 PNG 的 base64
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let url = format!("data:image/png;base64,{b64}");
        let bytes = decode_data_url(&url).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert!(decode_data_url("not-a-data-url").is_err());
    }
}
