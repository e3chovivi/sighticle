//! 录屏管线：xcap 抓取整屏帧 → 按选区裁剪 → FFmpeg 编码为 MP4
//!
//! 设计说明：
//! - Windows 上 xcap 底层走 Windows.Graphics.Capture，帧为整屏 BGRA；
//!   区域录制 = 整屏抓帧 + 软件裁剪，避免依赖尚不完善的窗口级录制。
//! - 原始 RGB 帧通过管道喂给 FFmpeg（libx264），不落地中间帧文件。
//! - 选区坐标由调用方换算为全局物理像素后传入（见 lib.rs::overlay_physical_region），
//!   本模块只负责"物理区域 → 显示器内裁剪"的收敛，天然兼容任意多屏/混合缩放布局。
//! - **所有 xcap WGC 操作（截图 capture_region 与录屏 video_recorder）固定在同一条
//!   专用工作线程上串行执行**：xcap 内部的 D3D 设备/立即上下文/显示器采集项均为
//!   进程级全局静态（LazyLock），并发或跨线程使用会报"参数错误"。截图原先跑在
//!   Tauri 线程池、录屏跑在本线程，两者交错即触发该错误——这是第二次录屏失败的
//!   根因。start/stop/capture 命令统一经通道转发到本线程，物理上杜绝并发。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use xcap::{Monitor, VideoRecorder};

use base64::Engine as _;

const FPS: u32 = 30;

/// 屏幕采集启动的最大尝试次数。连续启停后偶发失败（上一次 WGC 会话资源尚在异步释放），
/// 退避重试可自行恢复。6 次 × 600ms ≈ 最多 3 秒重试窗口：
/// 失败是**瞬时**的就能自愈，失败是**持续**的也会把每次原因完整写进日志。
const WGC_ATTEMPTS: usize = 6;
/// 每次重试之间的退避步长
const WGC_RETRY_STEP_MS: u64 = 600;

/// 上一次录制停止的时刻。诊断价值：排查"第二次启动失败"时，
/// 「距上次停止过了多久」是关键线索（设备/WGC 资源释放需要时间）。
static LAST_STOP_AT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn last_stop_at() -> &'static Mutex<Option<Instant>> {
    LAST_STOP_AT.get_or_init(|| Mutex::new(None))
}

/// 距上次录制停止的毫秒数（从未停止过则为 None）
fn since_last_stop_ms() -> Option<u128> {
    last_stop_at()
        .lock()
        .ok()
        .and_then(|g| g.map(|t| t.elapsed().as_millis()))
}

/// 一次进行中的录制。
/// 屏幕模式持有 WGC 采集器与帧搬运线程；摄像头模式（FFmpeg dshow 直录）两者皆为 None。
struct Active {
    recorder: Option<VideoRecorder>,
    ffmpeg: Child,
    writer: Option<JoinHandle<()>>,
    /// 置位后帧线程立即退出（xcap 停止抓帧后通道不会关闭，必须靠标志位打断阻塞接收）
    stop_flag: Option<Arc<AtomicBool>>,
    tmp_path: PathBuf,
    started: Instant,
    /// "record-mp4"（GUI 屏幕录制）| "cli" | "camera"——do_stop 据此选择不同的 ffmpeg 收尾方式
    mode: String,
}

/// 启动录制时返回的显示器信息（物理像素），预留供前端使用
#[derive(Debug, serde::Serialize)]
pub struct StartInfo {
    pub mon_x: i32,
    pub mon_y: i32,
    pub mon_w: u32,
    pub mon_h: u32,
    pub scale: f64,
    /// 实际启用成功的音频轨道（控制条据此展示）
    pub mic: bool,
    pub sys_audio: bool,
    /// 音频启动告警（如设备缺失），非致命，仅提示
    pub audio_warnings: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct StopResult {
    /// discard 时为 None；否则为临时录制文件路径（由前端编辑器选段后导出）
    pub path: Option<String>,
    pub seconds: f64,
}

// ================= 专用工作线程（保证 WGC/D3D 全局状态的单线程串行使用） =================

enum Cmd {
    Start {
        /// 录制临时文件目录（由调用方决定：GUI 用应用数据目录，CLI 用同款目录）
        dir: PathBuf,
        mode: String,
        px: i32,
        py: i32,
        pw: u32,
        ph: u32,
        /// 是否采集麦克风 / 系统声音
        mic: bool,
        sys_audio: bool,
        reply: SyncSender<Result<StartInfo, String>>,
    },
    Stop {
        discard: bool,
        reply: SyncSender<Result<StopResult, String>>,
    },
    Capture {
        px: i32,
        py: i32,
        pw: u32,
        ph: u32,
        reply: SyncSender<Result<crate::ScreenshotData, String>>,
    },
}

static CMD_TX: OnceLock<SyncSender<Cmd>> = OnceLock::new();

fn cmd_tx() -> &'static SyncSender<Cmd> {
    CMD_TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<Cmd>(8);
        std::thread::Builder::new()
            .name("lc-recorder".into())
            .spawn(move || worker(rx))
            .expect("无法启动录制工作线程");
        tx
    })
}

/// 工作线程：串行处理 Start/Stop/Capture，持有全部录制状态。
/// xcap 的 D3D 设备/上下文/采集项为进程级全局静态，只有全部收敛到本线程串行执行，
/// 才能彻底避免跨线程/并发使用导致的"参数错误"。
fn worker(rx: std::sync::mpsc::Receiver<Cmd>) {
    let mut active: Option<Active> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Start {
                dir,
                mode,
                px,
                py,
                pw,
                ph,
                mic,
                sys_audio,
                reply,
            } => {
                let res = if active.is_some() {
                    Err("已有录制在进行".into())
                } else {
                    match do_start(&dir, mode, px, py, pw, ph, mic, sys_audio) {
                        Ok((info, a)) => {
                            active = Some(a);
                            Ok(info)
                        }
                        Err(e) => Err(e),
                    }
                };
                let _ = reply.send(res);
            }
            Cmd::Stop { discard, reply } => {
                let res = match active.take() {
                    Some(a) => do_stop(a, discard),
                    None => Err("没有进行中的录制".to_string()),
                };
                let _ = reply.send(res);
            }
            Cmd::Capture {
                px,
                py,
                pw,
                ph,
                reply,
            } => {
                // 录制进行中拒绝截图：避免与帧回调并发使用全局 D3D 上下文
                let res = if active.is_some() {
                    Err("录制进行中，暂不能截图".into())
                } else {
                    do_capture(px, py, pw, ph)
                };
                let _ = reply.send(res);
            }
        }
    }
}

/// 拼装 FFmpeg 编码参数：原始 RGB 管道输入 → H.264 MP4
pub fn build_ffmpeg_args(w: u32, h: u32, out: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        "-s".into(),
        format!("{w}x{h}"),
        "-r".into(),
        FPS.to_string(),
        "-i".into(),
        "pipe:0".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "23".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-movflags".into(),
        "+faststart".into(),
        out.to_string_lossy().into_owned(),
    ]
}

/// 摄像头录制参数：FFmpeg dshow 直接读摄像头编码为 MP4。
/// 音频不在此处理（由独立线程写 WAV，停止后统一混流），
/// 分辨率/帧率用设备默认值——不同摄像头能力差异大，强制指定易失败。
/// stdin 保留为 piped：停止时写 `q` 让 FFmpeg 优雅收尾（moov 必须正常写入，否则 mp4 损坏）。
pub fn build_camera_args(device: &str, out: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-f".into(),
        "dshow".into(),
        "-i".into(),
        format!("video={device}"),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "23".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-movflags".into(),
        "+faststart".into(),
        out.to_string_lossy().into_owned(),
    ]
}

/// 从 `ffmpeg -list_devices -f dshow` 的 stderr 中提取视频设备名。
/// 兼容两代输出格式（设备行都含引号对，靠标记与分段区分视频/音频）：
/// - ffmpeg 8.x 新格式：`[in#0 @ ...] "HD User Facing" (video)` —— 行尾显式 (video)/(audio) 标记；
/// - 旧格式：`DirectShow video devices` 分段标题 + 无标记的 `"设备名"` 行。
pub fn parse_dshow_video_devices(stderr: &str) -> Vec<String> {
    let mut in_video_section = false;
    let mut out = Vec::new();
    for line in stderr.lines() {
        if line.contains("DirectShow video devices") {
            in_video_section = true;
            continue;
        }
        if line.contains("DirectShow audio devices") {
            in_video_section = false;
            continue;
        }
        // Alternative name 行也带引号，必须先跳过
        if line.contains("Alternative name") {
            continue;
        }
        // 提取首个引号对作为设备名，引号之后的内容作为类型标记
        let Some(q1) = line.find('"') else { continue };
        let Some(q2) = line[q1 + 1..].find('"') else {
            continue;
        };
        let name = &line[q1 + 1..q1 + 1 + q2];
        let after = &line[q1 + 1 + q2 + 1..];
        if name.is_empty() {
            continue;
        }
        if after.contains("(audio)") {
            continue; // 显式音频标记
        }
        if after.contains("(video)") || in_video_section {
            out.push(name.to_string());
        }
    }
    out
}

/// 枚举 dshow 视频设备并返回第一个（多个摄像头时取列表首位，全部记入日志）
pub fn dshow_video_device() -> Result<String, String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
        .output()
        .map_err(|e| format!("调用 ffmpeg 枚举摄像头失败：{e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let devices = parse_dshow_video_devices(&stderr);
    crate::lclog!("dshow 摄像头设备列表：{devices:?}");
    devices
        .into_iter()
        .next()
        .ok_or_else(|| "未找到摄像头设备（可能无摄像头或被系统禁用）".to_string())
}

/// 拼装导出参数：从临时录制文件裁剪 [start, end] 段。
/// mp4 重编码（帧级精确）；gif 走 palette 两步法并限制宽度 ≤720、12fps 控制体积。
pub fn build_export_args(src: &str, start: f64, end: f64, dest: &str, format: &str) -> Vec<String> {
    let ss = format!("{start:.3}");
    let dur = format!("{:.3}", end - start);
    let mut args = vec!["-y".to_string(), "-ss".to_string(), ss, "-i".to_string(), src.to_string(), "-t".to_string(), dur];
    if format == "gif" {
        // GIF 容器无音轨
        args.push("-an".into());
        args.push("-vf".into());
        args.push(
            "fps=12,scale=min(720\\,iw):-1:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse"
                .into(),
        );
    } else {
        args.extend([
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            "veryfast".into(),
            "-crf".into(),
            "23".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            // 保留音频（源无音轨时 FFmpeg 会自动忽略）
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            "192k".into(),
            "-movflags".into(),
            "+faststart".into(),
        ]);
    }
    args.push(dest.to_string());
    args
}

/// 拼装"把音频混入视频"的参数：视频流直接 copy（不重编码，快且无损），
/// 音频统一转 AAC。
///
/// `audios` 为 (音频文件, 增益) 列表：增益来自用户设置——麦克风离得远导致偏小时可以调大。
/// 用 `volume=` 滤波器而非 `-af`，便于对多路分别施加不同增益；
/// `normalize=0` 保持原始音量（否则 FFmpeg 会按路数把音量衰减一半）。
pub fn build_mux_args(video: &str, audios: &[(String, f32)], out: &str) -> Vec<String> {
    let mut args = vec!["-y".to_string(), "-i".to_string(), video.to_string()];
    for (a, _) in audios {
        args.push("-i".into());
        args.push(a.clone());
    }
    let first_gain = audios.first().map(|x| x.1).unwrap_or(1.0);
    let filter = if audios.len() >= 2 {
        format!(
            "[1:a]volume={:.2}[m];[2:a]volume={:.2}[s];[m][s]amix=inputs=2:duration=longest:normalize=0[a]",
            audios[0].1, audios[1].1
        )
    } else {
        format!("[1:a]volume={first_gain:.2}[a]")
    };
    args.extend([
        "-filter_complex".into(),
        filter,
        "-map".into(),
        "0:v".into(),
        "-map".into(),
        "[a]".into(),
        "-c:v".into(),
        "copy".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
    ]);
    args.push(out.to_string());
    args
}

/// 同步执行一次 FFmpeg，带超时兜底（超时即强杀，绝不让界面卡死）
pub(crate) fn run_ffmpeg(args: &[String], timeout: Duration) -> Result<(), String> {
    let mut child = Command::new("ffmpeg")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err("FFmpeg 处理失败".into())
                };
            }
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    return Err("FFmpeg 执行超时".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// 将裁剪区域（相对显示器左上角的物理像素，可为负/越界）收敛到显示器范围内。
/// 收敛后宽或高 < 2 像素返回 None。
pub fn clamp_crop(x: i32, y: i32, w: u32, h: u32, mon_w: u32, mon_h: u32) -> Option<(u32, u32, u32, u32)> {
    let mut x = x as i64;
    let mut y = y as i64;
    let mut w = w as i64;
    let mut h = h as i64;
    if x < 0 {
        w += x;
        x = 0;
    }
    if y < 0 {
        h += y;
        y = 0;
    }
    w = w.min(mon_w as i64 - x);
    h = h.min(mon_h as i64 - y);
    if w < 2 || h < 2 {
        return None;
    }
    Some((x as u32, y as u32, w as u32, h as u32))
}

/// 从整屏帧中裁剪指定区域并转为 RGB。
///
/// 通道顺序：xcap 的 `Frame.raw` 已经是 **RGBA**——xcap 内部 `texture_to_frame` 结尾
/// 调用 `bgra_to_rgba()` 做过一次通道交换。这里**绝不能再交换 R/B**，否则画面整体偏色
/// （曾经因为多交换一次，紫色被还原成了黄色）。
fn crop_frame_to_rgb(raw: &[u8], frame_w: u32, frame_h: u32, x: u32, y: u32, w: u32, h: u32) -> Option<Vec<u8>> {
    if x + w > frame_w || y + h > frame_h {
        return None;
    }
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for row in y..(y + h) {
        let base = (row * frame_w + x) as usize * 4;
        for px in 0..w as usize {
            let i = base + px * 4;
            // RGBA -> RGB（保持通道顺序，不做交换）
            out.push(raw[i]);
            out.push(raw[i + 1]);
            out.push(raw[i + 2]);
        }
    }
    Some(out)
}

// ================= 对外 API（转发到工作线程） =================

/// 开始录制。px/py/pw/ph 为全局物理像素坐标（已由覆盖层窗口换算）；
/// mic / sys_audio 指定是否采集对应音频轨道。
pub fn start(
    dir: PathBuf,
    mode: String,
    px: i32,
    py: i32,
    pw: u32,
    ph: u32,
    mic: bool,
    sys_audio: bool,
) -> Result<StartInfo, String> {
    let (tx, rx) = sync_channel(1);
    cmd_tx()
        .send(Cmd::Start {
            dir,
            mode,
            px,
            py,
            pw,
            ph,
            mic,
            sys_audio,
            reply: tx,
        })
        .map_err(|_| "录制工作线程未运行".to_string())?;
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(r) => r,
        Err(_) => {
            crate::lclog!("!! 等待录制线程的启动回复超时（20 秒）");
            Err("启动录制超时".to_string())
        }
    }
}

/// 停止录制。discard=true 删除临时文件；否则返回临时文件路径供前端编辑器导出。
pub fn stop(discard: bool) -> Result<StopResult, String> {
    let (tx, rx) = sync_channel(1);
    cmd_tx()
        .send(Cmd::Stop { discard, reply: tx })
        .map_err(|_| "录制工作线程未运行".to_string())?;
    rx.recv_timeout(Duration::from_secs(25))
        .map_err(|_| "停止录制超时".to_string())?
}

/// 区域截图（同样经工作线程执行，见模块头说明）。
pub fn capture(px: i32, py: i32, pw: u32, ph: u32) -> Result<crate::ScreenshotData, String> {
    let (tx, rx) = sync_channel(1);
    cmd_tx()
        .send(Cmd::Capture {
            px,
            py,
            pw,
            ph,
            reply: tx,
        })
        .map_err(|_| "采集工作线程未运行".to_string())?;
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| "截图超时".to_string())?
}

/// 导出录屏：裁剪 [start,end] 段编码为 MP4/GIF，写入 dest。
/// 阻塞等待 FFmpeg（最长 180 秒），调用方应放后台线程。
pub fn export(src: &str, start: f64, end: f64, dest: &str, format: &str) -> Result<String, String> {
    if end - start < 0.05 {
        crate::lclog!("导出被拒：选段过短（{start:.3}–{end:.3}）");
        return Err("选段时长过短".into());
    }
    let args = build_export_args(src, start, end, dest, format);
    crate::lclog!(
        "开始导出：format={format} 选段={start:.3}–{end:.3}s 目标={dest}\n  ffmpeg {}",
        args.join(" ")
    );
    run_ffmpeg(&args, Duration::from_secs(180)).map_err(|e| {
        crate::lclog!("!! 导出失败：{e}");
        format!("导出失败：{e}")
    })?;
    crate::lclog!("导出成功：{dest}");
    Ok(dest.to_string())
}

/// 放弃录制：删除临时文件（不存在视为成功）
pub fn discard(src: &str) -> Result<(), String> {
    match std::fs::remove_file(src) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

// ================= 工作线程内部实现 =================

/// 按已录制的真实时长计算"到此刻为止应写出的总帧数"。
///
/// 抽成纯函数便于单测。存在意义：Windows Graphics Capture 是**画面变化驱动**投帧的，
/// 静止画面几乎不投帧（实测录 10 秒只收到 76 帧）；而 FFmpeg 的 rawvideo 管道输入
/// 只能按固定帧率解释字节流，若直接透传，76 帧会被当作 76/30 ≈ 2.5 秒，成片时长
/// 严重短于真实录制时长。因此必须按真实时间轴把帧补齐。
pub fn frames_for_elapsed(elapsed_ms: u64, fps: u32) -> u64 {
    (elapsed_ms * fps as u64) / 1000
}

/// 计算本轮循环还需补写多少帧（0 表示当前无需写出）。
/// 限幅 `2×fps`，避免编码卡顿后一次性爆发补写造成长时间阻塞。
pub fn frames_to_emit(elapsed_ms: u64, emitted: u64, fps: u32) -> u64 {
    let target = frames_for_elapsed(elapsed_ms, fps).min(emitted + fps as u64 * 2);
    target.saturating_sub(emitted)
}

fn do_start(
    dir: &Path,
    mode: String,
    px: i32,
    py: i32,
    pw: u32,
    ph: u32,
    mic: bool,
    sys_audio: bool,
) -> Result<(StartInfo, Active), String> {
    let since = since_last_stop_ms()
        .map(|ms| format!("{ms}ms"))
        .unwrap_or_else(|| "无（本次为首次录制）".into());
    crate::lclog!(
        "开始录制：mode={mode} 物理区域=({px},{py} {pw}x{ph}) mic={mic} sys={sys_audio} 距上次停止={since}"
    );

    // FFmpeg 可用性检查（开发期使用系统 PATH，后续版本打包内置）
    if let Err(e) = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        crate::lclog!("FFmpeg 不可用：{e}");
        return Err("未找到 ffmpeg：请先安装并加入 PATH".to_string());
    }

    // 摄像头录制：FFmpeg dshow 直录，不经 WGC/补帧管线（无屏幕区域概念）
    if mode == "camera" {
        return do_start_camera(dir, mic, sys_audio);
    }

    let mon = Monitor::from_point(px + pw as i32 / 2, py + ph as i32 / 2).map_err(|e| {
        crate::lclog!("选择显示器失败：{e:?}");
        e.to_string()
    })?;
    let scale = mon.scale_factor().map_err(|e| e.to_string())? as f64;
    let mon_x = mon.x().map_err(|e| e.to_string())?;
    let mon_y = mon.y().map_err(|e| e.to_string())?;
    let mon_w = mon.width().map_err(|e| e.to_string())?;
    let mon_h = mon.height().map_err(|e| e.to_string())?;
    let (cx0, cy0, cw, ch) = clamp_crop(px - mon_x, py - mon_y, pw, ph, mon_w, mon_h)
        .ok_or_else(|| "选区超出屏幕范围".to_string())?;
    crate::lclog!(
        "显示器：({mon_x},{mon_y}) {mon_w}x{mon_h} scale={scale}；裁切区域=({cx0},{cy0} {cw}x{ch})"
    );

    let dir = dir.to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp_path = dir.join(format!("tmp_{}.mp4", crate::stamp()));

    let args = build_ffmpeg_args(cw, ch, &tmp_path);
    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().expect("ffmpeg stdin 不可用");

    // WGC 采集启动：最多重试 4 次（300/600/900ms 退避）。
    // 连续启停后立即重启偶发失败（上一次会话的采集资源尚在异步释放），稍候即可恢复。
    // 关键日志：把 video_recorder()（创建采集器）与 start()（真正开流）的失败**分开记录**，
    // 两者失败原因完全不同，混在一起无法定位。
    let (recorder, rx) = {
        let mut last_err = String::new();
        let mut got = None;
        for attempt in 1..=WGC_ATTEMPTS {
            let t0 = Instant::now();
            match mon.video_recorder() {
                Ok((rec, ch)) => match rec.start() {
                    Ok(()) => {
                        crate::lclog!(
                            "屏幕采集启动成功（第 {attempt}/{WGC_ATTEMPTS} 次尝试，耗时 {}ms）",
                            t0.elapsed().as_millis()
                        );
                        got = Some((rec, ch));
                        break;
                    }
                    Err(e) => {
                        last_err = format!("StartCapture 失败：{e:?}");
                        crate::lclog!(
                            "屏幕采集 start() 失败（第 {attempt}/{WGC_ATTEMPTS} 次）：{e:?}"
                        );
                        // 显式丢弃本次创建的对象，确保 WGC 会话/帧池被释放
                        drop(ch);
                        drop(rec);
                    }
                },
                Err(e) => {
                    last_err = format!("创建采集器失败：{e:?}");
                    crate::lclog!(
                        "video_recorder() 失败（第 {attempt}/{WGC_ATTEMPTS} 次）：{e:?}"
                    );
                }
            }
            // 最后一次失败后无需再等，直接给出结论
            if attempt < WGC_ATTEMPTS {
                std::thread::sleep(Duration::from_millis(WGC_RETRY_STEP_MS));
            }
        }
        match got {
            Some(v) => v,
            None => {
                // 启动失败：清理本次已拉起的 ffmpeg 与临时文件，不留孤儿进程/文件
                crate::lclog!("屏幕采集连续 {WGC_ATTEMPTS} 次启动失败，放弃本次录制：{last_err}");
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&tmp_path);
                return Err(format!("启动屏幕采集失败：{last_err}"));
            }
        }
    };

    // 音频采集：放在屏幕采集成功之后启动，避免屏幕采集失败时留下一堆音频临时文件。
    // 音频属于"锦上添花"，任一路失败都只记告警，绝不阻断视频录制。
    // 注意：音频运行在**独立专用线程**上（见 audio.rs）——cpal/WASAPI 与 xcap/WGC 的
    // COM 套间状态按线程保存，两者混在同一条线程会导致第二次录制启动屏幕采集时报参数错误。
    let audio_report = crate::audio::start_capture(&dir, mic, sys_audio);
    crate::lclog!(
        "音频采集结果：麦克风={} 系统声音={} 告警={:?}",
        audio_report.mic,
        audio_report.sys,
        audio_report.warnings
    );

    // 帧搬运线程：把"变化驱动"的原始帧转成**按墙钟节奏的恒定帧率流**。
    //
    // 为什么必须补帧：WGC 只在画面变化时投帧，静止画面几乎不投（实测 10 秒仅 76 帧）。
    // 而 FFmpeg rawvideo 管道只能按声明帧率解释字节流，直接透传会导致成片时长严重偏短
    // （76 帧被当成 2.5 秒）。这里改为按真实时间轴输出：每次收到帧就更新"最新画面"，
    // 并按 FPS 节拍把最新画面写出（静止期重复上一帧），保证成片时长 == 录制时长。
    let stop_flag = Arc::new(AtomicBool::new(false));
    let flag = stop_flag.clone();
    let writer = std::thread::spawn(move || {
        let period = Duration::from_micros(1_000_000 / FPS as u64);
        let start = Instant::now();
        let mut emitted: u64 = 0;
        let mut latest: Option<Vec<u8>> = None;
        loop {
            if flag.load(Ordering::Relaxed) {
                break;
            }
            match rx.recv_timeout(period) {
                Ok(frame) => {
                    if let Some(rgb) =
                        crop_frame_to_rgb(&frame.raw, frame.width, frame.height, cx0, cy0, cw, ch)
                    {
                        latest = Some(rgb);
                    }
                }
                // 静止期没有新帧：继续推进时间轴（下方会用最新画面补齐）
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let Some(rgb) = latest.as_ref() else {
                continue;
            };
            // 按真实时间轴补写：静止期重复最新画面，使成片时长 == 录制时长
            let mut due =
                frames_to_emit(start.elapsed().as_millis() as u64, emitted, FPS);
            while due > 0 {
                if stdin.write_all(rgb).is_err() {
                    return;
                }
                emitted += 1;
                due -= 1;
            }
        }
    });

    let active = Active {
        recorder: Some(recorder),
        ffmpeg: child,
        writer: Some(writer),
        stop_flag: Some(stop_flag),
        tmp_path,
        started: Instant::now(),
        mode,
    };

    Ok((
        StartInfo {
            mon_x,
            mon_y,
            mon_w,
            mon_h,
            scale,
            mic: audio_report.mic,
            sys_audio: audio_report.sys,
            audio_warnings: audio_report.warnings,
        },
        active,
    ))
}

/// 摄像头录制启动：FFmpeg dshow 直录到临时 mp4，不经 WGC 采集与补帧线程。
/// 音频仍走独立线程 WAV（与屏幕录制同一条管线），停止时统一混流。
fn do_start_camera(dir: &Path, mic: bool, sys_audio: bool) -> Result<(StartInfo, Active), String> {
    crate::lclog!("开始摄像头录制：mic={mic} sys={sys_audio}");
    let device = dshow_video_device()?;
    let dir = dir.to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp_path = dir.join(format!("tmp_{}.mp4", crate::stamp()));

    let args = build_camera_args(&device, &tmp_path);
    crate::lclog!("摄像头录制 ffmpeg {}", args.join(" "));
    let mut child = Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动 FFmpeg 摄像头录制失败：{e}"))?;

    // 启动验证：dshow 打不开设备时 ffmpeg 会迅速退出并留下残缺文件。
    // 这里短暂等待后检查进程是否还活着；失败则清理并给出明确原因。
    std::thread::sleep(Duration::from_millis(700));
    match child.try_wait() {
        Ok(Some(status)) => {
            let _ = std::fs::remove_file(&tmp_path);
            crate::lclog!("摄像头打开失败，FFmpeg 立即退出：{status}");
            return Err(format!(
                "摄像头打开失败（{status}）：可能被其它程序占用或设备不可用"
            ));
        }
        Ok(None) => {} // 正常录制中
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!("检查摄像头进程状态失败：{e}"));
        }
    }
    crate::lclog!("摄像头已开启：{device}");

    // 音频与屏幕录制共用（独立线程，任一路失败仅告警）
    let audio_report = crate::audio::start_capture(&dir, mic, sys_audio);
    crate::lclog!(
        "音频采集结果：麦克风={} 系统声音={} 告警={:?}",
        audio_report.mic,
        audio_report.sys,
        audio_report.warnings
    );

    let active = Active {
        recorder: None,
        ffmpeg: child,
        writer: None,
        stop_flag: None,
        tmp_path,
        started: Instant::now(),
        mode: "camera".into(),
    };
    Ok((
        StartInfo {
            // 摄像头录制不依赖屏幕几何，字段填 0 仅作占位
            mon_x: 0,
            mon_y: 0,
            mon_w: 0,
            mon_h: 0,
            scale: 1.0,
            mic: audio_report.mic,
            sys_audio: audio_report.sys,
            audio_warnings: audio_report.warnings,
        },
        active,
    ))
}

fn do_capture(px: i32, py: i32, pw: u32, ph: u32) -> Result<crate::ScreenshotData, String> {
    let mon = Monitor::from_point(px + pw as i32 / 2, py + ph as i32 / 2).map_err(|e| e.to_string())?;
    let mon_x = mon.x().map_err(|e| e.to_string())?;
    let mon_y = mon.y().map_err(|e| e.to_string())?;
    let mon_w = mon.width().map_err(|e| e.to_string())?;
    let mon_h = mon.height().map_err(|e| e.to_string())?;
    let (x, y, w, h) = clamp_crop(px - mon_x, py - mon_y, pw, ph, mon_w, mon_h)
        .ok_or_else(|| "选区超出屏幕范围".to_string())?;
    let img = mon.capture_region(x, y, w, h).map_err(|e| e.to_string())?;
    let (width, height) = (img.width(), img.height());
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    Ok(crate::ScreenshotData {
        width,
        height,
        data_url: format!("data:image/png;base64,{b64}"),
    })
}

fn do_stop(active: Active, discard: bool) -> Result<StopResult, String> {
    let seconds = active.started.elapsed().as_secs_f64();
    let is_camera = active.mode == "camera";
    crate::lclog!(
        "停止录制：mode={} discard={discard} 已录 {seconds:.2}s",
        active.mode
    );
    // 停止顺序（屏幕模式）：置停止标志 → 停止抓帧 → 限时等帧线程退出（超时则分离，绝不卡死）
    // → 关闭管道让 FFmpeg 收到 EOF → 限时等封装完成（超时则强杀）
    // 摄像头模式无采集器与帧线程，仅需优雅停止 FFmpeg（写 q），后半段收尾逻辑两者共用。
    if let Some(flag) = active.stop_flag.as_ref() {
        flag.store(true, Ordering::Relaxed);
    }
    if let Some(rec) = active.recorder.as_ref() {
        let _ = rec.stop();
    }
    let Active {
        recorder: _recorder,
        mut ffmpeg,
        writer,
        stop_flag: _flag,
        tmp_path,
        started: _started,
        mode: _mode,
    } = active;
    // 记录停止时刻：下次启动会据此打印"距上次停止多久"，是排查二次启动失败的关键线索
    if let Ok(mut g) = last_stop_at().lock() {
        *g = Some(Instant::now());
    }

    // 立刻停止音频采集：让音频与视频覆盖同一时间区间，并尽早释放 WASAPI 设备
    // （音频在专用线程上运行，这里只发命令并等其收尾，全程限时；无会话则返回空）
    let audio_stop_t0 = Instant::now();
    let (audio_files, empty_files): (Vec<PathBuf>, Vec<PathBuf>) = crate::audio::stop_capture()
        .into_iter()
        .partition(|p| crate::audio::has_audio_data(p));
    crate::lclog!(
        "音频已停止（耗时 {}ms）：有效轨 {:?}，空轨 {:?}",
        audio_stop_t0.elapsed().as_millis(),
        audio_files,
        empty_files
    );

    if let Some(writer) = writer {
        let join_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if writer.is_finished() {
                let _ = writer.join();
                crate::lclog!("视频帧线程已退出");
                break;
            }
            if Instant::now() > join_deadline {
                // 帧线程异常挂起：分离它，继续收尾流程，保证界面不被拖死
                crate::lclog!("!! 视频帧线程 3 秒内未退出，放弃等待并分离（可能有资源未释放）");
                std::mem::forget(writer);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    // 收尾 FFmpeg：屏幕模式关闭 stdin 给 EOF；
    // 摄像头模式写 `q` 让 FFmpeg 优雅退出——mp4 的 moov 必须正常写入，
    // 直接强杀会得到损坏文件。两者随后同样限时等待、超时强杀。
    if is_camera {
        if let Some(mut si) = ffmpeg.stdin.take() {
            let _ = si.write_all(b"q");
            let _ = si.flush();
        }
    } else {
        drop(ffmpeg.stdin.take());
    }
    let wait_deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match ffmpeg.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > wait_deadline {
                    crate::lclog!("!! FFmpeg 8 秒内未结束，强制结束进程");
                    let _ = ffmpeg.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }

    let cleanup_audio = |files: &[PathBuf]| {
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    };
    cleanup_audio(&empty_files);

    if discard {
        let _ = std::fs::remove_file(&tmp_path);
        cleanup_audio(&audio_files);
        crate::lclog!("录制已放弃，临时文件清理完毕");
        return Ok(StopResult { path: None, seconds });
    }

    // 有音频就把它混入视频（视频流直接 copy，不重编码），产出带声文件供预览与裁剪；
    // 混流失败则回退为无声音视频，保证录制成果不丢。
    let working = if audio_files.is_empty() {
        crate::lclog!("无有效音频轨，输出无声视频");
        tmp_path.clone()
    } else {
        let muxed = tmp_path.with_file_name(format!("tmp_av_{}.mp4", crate::stamp()));
        // 按文件名判定来源再套用增益：避免"某一路为空被丢弃"导致增益张冠李戴
        let st = crate::settings::get();
        let tracks: Vec<(String, f32)> = audio_files
            .iter()
            .map(|p| {
                let path = p.to_string_lossy().into_owned();
                let gain = if path.ends_with("mic.wav") {
                    st.mic_gain
                } else {
                    st.sys_gain
                };
                (path, gain)
            })
            .collect();
        let args = build_mux_args(&tmp_path.to_string_lossy(), &tracks, &muxed.to_string_lossy());
        crate::lclog!("开始混流：ffmpeg {}", args.join(" "));
        match run_ffmpeg(&args, Duration::from_secs(60)) {
            Ok(()) => {
                let _ = std::fs::remove_file(&tmp_path);
                cleanup_audio(&audio_files);
                crate::lclog!("混流成功：{}", muxed.display());
                muxed
            }
            Err(e) => {
                crate::lclog!("!! 音频混流失败，回退为无声视频：{e}");
                let _ = std::fs::remove_file(&muxed);
                cleanup_audio(&audio_files);
                tmp_path.clone()
            }
        }
    };

    // 不直接落盘到视频目录：返回临时文件路径，由前端编辑器选段后决定导出目标
    crate::lclog!("录制收尾完成：{}", working.display());
    Ok(StopResult {
        path: Some(working.to_string_lossy().into_owned()),
        seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_args_shape() {
        let args = build_ffmpeg_args(640, 480, Path::new("out.mp4"));
        assert!(args.contains(&"640x480".to_string()));
        assert!(args.contains(&"libx264".to_string()));
        assert!(args.contains(&"pipe:0".to_string()));
        assert_eq!(args.last().unwrap(), "out.mp4");
    }

    #[test]
    fn camera_args_shape() {
        let args = build_camera_args("Integrated Camera", Path::new("cam.mp4"));
        assert!(args.contains(&"dshow".to_string()));
        assert!(
            args.contains(&"video=Integrated Camera".to_string()),
            "输入应为 dshow 的 video=设备名"
        );
        assert!(args.contains(&"libx264".to_string()));
        assert_eq!(args.last().unwrap(), "cam.mp4");
        assert!(
            !args.contains(&"mjpeg".to_string()),
            "已移除预览帧第二输出（用户要求砍掉实时预览）"
        );
    }

    #[test]
    fn parses_dshow_video_devices() {
        let stderr = r#"
[dshow @ 000001c00000] DirectShow video devices (some may be both video and audio devices)
[dshow @ 000001c00000]  "Integrated Camera"
[dshow @ 000001c00000]     Alternative name "@device_pnp_\\?\usb#vid_04f2&pid_b6d2&mi_00#6&2f0f1a5f&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global"
[dshow @ 000001c00000]  "OBS Virtual Camera"
[dshow @ 000001c00000] DirectShow audio devices (some may be both video and audio devices)
[dshow @ 000001c00000]  "Microphone Array (Realtek)"
"#;
        let devices = parse_dshow_video_devices(stderr);
        assert_eq!(
            devices,
            vec!["Integrated Camera".to_string(), "OBS Virtual Camera".to_string()],
            "应取到两个视频设备且跳过 Alternative name 行"
        );
    }

    #[test]
    fn parses_dshow_no_devices() {
        assert!(
            parse_dshow_video_devices("No matching device found").is_empty(),
            "无视频段时返回空"
        );
        assert!(
            parse_dshow_video_devices(
                "[dshow @ x] DirectShow video devices (some may be both video and audio devices)"
            )
            .is_empty(),
            "只有标题没有设备时返回空"
        );
    }

    /// ffmpeg 8.x 新格式（无分段标题，行尾 (video)/(audio) 标记）
    #[test]
    fn parses_dshow_ffmpeg8_format() {
        let stderr = r#"
[in#0 @ 0000014e57c4b340] "HD User Facing" (video)
[in#0 @ 0000014e57c4b340]   Alternative name "@device_pnp_\\?\usb#vid_0408&pid_a061&mi_00#...\global"
[in#0 @ 0000014e57c4b340] "麦克风阵列 (Realtek(R) Audio)" (audio)
[in#0 @ 0000014e57c4b340]   Alternative name "@device_cm_{33D9A762-90C8-11D0-BD43-00A0C911CE86}\wave_{ADE681BB-10B2-4B20-888C-0B51437521CD}"
"#;
        assert_eq!(
            parse_dshow_video_devices(stderr),
            vec!["HD User Facing".to_string()],
            "应取到视频设备、跳过音频与 Alternative name 行"
        );
    }

    /// 回归测试：变化驱动投帧时，成片帧数必须由真实时长决定，而非"收到多少帧就写多少帧"。
    /// 曾经的 bug：录 10 秒只收到 76 帧并直接透传，成片只有 2.53 秒。
    #[test]
    fn frames_follow_wall_clock_not_arrival_count() {
        // 录 10 秒：即使只收到 76 帧，也应补足到 300 帧（10s × 30fps）
        assert_eq!(frames_for_elapsed(10_000, 30), 300);
        assert_eq!(frames_for_elapsed(2_533, 30), 75);
        // 边界
        assert_eq!(frames_for_elapsed(0, 30), 0);
        assert_eq!(frames_for_elapsed(1_000, 30), 30);
        assert_eq!(frames_for_elapsed(1_000, 15), 15);
    }

    /// 端到端模拟：画面几乎静止（10 秒仅 3 次变化投帧）时，按 30fps 节拍推进
    /// 最终也应产出约 10 秒（300 帧）的成片——这正是之前"10 秒录成 2.5 秒"的修复验证。
    #[test]
    fn sparse_frames_still_produce_full_length_video() {
        let total_ms = 10_000u64;
        let mut emitted = 0u64;
        let mut ms = 0u64;
        // 每约 33ms 一轮（recv 超时或被新帧唤醒），复刻 writer 循环的节奏
        while ms <= total_ms {
            emitted += frames_to_emit(ms, emitted, 30);
            ms += 33;
        }
        let expected = frames_for_elapsed(total_ms, 30);
        assert!(
            emitted * 100 >= expected * 99,
            "成片帧数 {emitted} 明显少于期望 {expected}（时长会被压缩）"
        );
        assert!(emitted <= expected, "不应超过真实时长对应的帧数");
    }

    /// 限幅保护：编码长时间卡顿后不应一次性爆发补写巨量帧
    #[test]
    fn frames_to_emit_is_rate_limited() {
        // 已写出 0 帧、却已过去 10 秒：单轮最多补 2×fps 帧
        assert_eq!(frames_to_emit(10_000, 0, 30), 60);
        // 正常节奏：约 33ms 推进 1 帧
        assert_eq!(frames_to_emit(34, 0, 30), 1);
        // 已跟上进度则无需补写
        assert_eq!(frames_to_emit(1_000, 30, 30), 0);
    }

    /// 单路音频：不混音，但必须施加该路的增益（麦克风偏小时用户会调大）
    #[test]
    fn mux_args_single_audio_applies_gain() {
        let audios = vec![("mic.wav".to_string(), 2.5f32)];
        let args = build_mux_args("v.mp4", &audios, "out.mp4");
        assert!(args.contains(&"copy".to_string()), "视频流应直接 copy 不重编码");
        assert!(args.contains(&"aac".to_string()));
        assert!(!args.iter().any(|a| a.contains("amix")), "单路不需要混音");
        let fc = args.iter().position(|a| a == "-filter_complex").unwrap();
        assert!(
            args[fc + 1].contains("volume=2.50"),
            "应施加麦克风增益：{}",
            args[fc + 1]
        );
        assert_eq!(args.last().unwrap(), "out.mp4");
    }

    /// 两路音频：各自施加增益后用 amix 混合，且不得自动衰减总音量
    #[test]
    fn mux_args_two_audios_apply_gains_and_amix() {
        let audios = vec![
            ("mic.wav".to_string(), 3.0f32),
            ("sys.wav".to_string(), 0.5f32),
        ];
        let args = build_mux_args("v.mp4", &audios, "out.mp4");
        let fc = args.iter().position(|a| a == "-filter_complex").unwrap();
        let filter = &args[fc + 1];
        assert!(filter.contains("volume=3.00"), "麦克风增益应生效：{filter}");
        assert!(filter.contains("volume=0.50"), "系统声音增益应生效：{filter}");
        assert!(filter.contains("amix=inputs=2"));
        assert!(filter.contains("normalize=0"), "混音不得自动衰减音量");
        assert!(args.contains(&"copy".to_string()));
        assert_eq!(args.last().unwrap(), "out.mp4");
    }

    /// GIF 容器不支持音频，导出时必须丢弃音轨
    #[test]
    fn gif_export_drops_audio() {
        let args = build_export_args("in.mp4", 0.0, 3.0, "out.gif", "gif");
        assert!(args.contains(&"-an".to_string()));
    }

    /// MP4 裁剪导出需保留音频（录屏带麦克风/系统声时不能丢）
    #[test]
    fn mp4_export_keeps_audio() {
        let args = build_export_args("in.mp4", 1.0, 4.0, "out.mp4", "mp4");
        assert!(!args.contains(&"-an".to_string()));
        assert!(args.contains(&"aac".to_string()));
    }

    #[test]
    fn export_args_mp4_trims_with_duration() {
        let args = build_export_args("in.mp4", 1.5, 6.0, "out.mp4", "mp4");
        let i = args.iter().position(|a| a == "-ss").unwrap();
        assert_eq!(args[i + 1], "1.500");
        let t = args.iter().position(|a| a == "-t").unwrap();
        assert_eq!(args[t + 1], "4.500");
        assert!(args.contains(&"libx264".to_string()));
        assert_eq!(args.last().unwrap(), "out.mp4");
    }

    #[test]
    fn export_args_gif_has_palette_filter() {
        let args = build_export_args("in.mp4", 0.0, 3.0, "out.gif", "gif");
        let v = args.iter().position(|a| a == "-vf").unwrap();
        let filter = &args[v + 1];
        assert!(filter.contains("palettegen"));
        assert!(filter.contains("paletteuse"));
        assert!(filter.contains("fps=12"));
        assert_eq!(args.last().unwrap(), "out.gif");
    }

    #[test]
    fn clamp_inside() {
        assert_eq!(clamp_crop(100, 100, 200, 150, 1920, 1080), Some((100, 100, 200, 150)));
    }

    #[test]
    fn clamp_right_bottom_overflow() {
        assert_eq!(clamp_crop(1900, 1060, 100, 100, 1920, 1080), Some((1900, 1060, 20, 20)));
    }

    #[test]
    fn clamp_negative_origin() {
        assert_eq!(clamp_crop(-10, -20, 100, 100, 1920, 1080), Some((0, 0, 90, 80)));
    }

    #[test]
    fn clamp_too_small_is_none() {
        assert_eq!(clamp_crop(1919, 1079, 10, 10, 1920, 1080), None);
        assert_eq!(clamp_crop(0, 0, 1, 500, 1920, 1080), None);
    }

    /// 回归：xcap 的帧是 **RGBA**（内部已做过 bgra_to_rgba）。
    /// 曾因再交换一次 R/B，导致整个录屏偏色（紫色被还原成黄色）。
    #[test]
    fn rgba_frame_is_converted_without_channel_swap() {
        // 2x2 帧，RGBA 存储：像素依次为 红 / 绿 / 蓝 / 白
        #[rustfmt::skip]
        let raw = vec![
            255, 0, 0, 255,    0, 255, 0, 255,
            0, 0, 255, 255,    255, 255, 255, 255,
        ];
        // 右列（绿、白）
        let right = crop_frame_to_rgb(&raw, 2, 2, 1, 0, 1, 2).unwrap();
        assert_eq!(right, vec![0, 255, 0, 255, 255, 255]);
        // 左列（红、蓝）：若发生 R/B 交换，红色会变成 (0,0,255)，此处即失败
        let left = crop_frame_to_rgb(&raw, 2, 2, 0, 0, 1, 2).unwrap();
        assert_eq!(
            left,
            vec![255, 0, 0, 0, 0, 255],
            "红色必须仍是红色——通道顺序不得交换"
        );
    }

    #[test]
    fn frame_crop_out_of_bounds_is_none() {
        let raw = vec![0u8; 4 * 4];
        assert!(crop_frame_to_rgb(&raw, 2, 2, 1, 1, 2, 2).is_none());
    }
}
