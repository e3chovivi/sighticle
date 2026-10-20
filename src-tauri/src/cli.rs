//! CLI 无头模式：给脚本 / AI 编排调用的截图与录屏入口。
//!
//! 用法（详见 `light-capture help`；机器可读能力声明 `light-capture help --json`）：
//!   light-capture screenshot --out shot.png [--region x,y,w,h]
//!   light-capture record --seconds 10 --out demo.mp4 [--region x,y,w,h] [--mic] [--sys-audio]
//!   light-capture record --camera --seconds 5 --out cam.mp4 [--mic]
//!   light-capture record --window "<标题>" --out demo.mp4 [--fps 15] [--scale 1280]
//!   light-capture record --stop-file stop.flag --out demo.mp4   # 随时停止（创建文件即停）
//!   light-capture list-windows --json          # 列出可见窗口，供 Agent 挑选录制目标
//!
//! 约定：
//! - stdout 只输出一行 JSON（机器可读），详细过程照常写本地日志文件；
//! - 成功退出码 0、失败 1；`--region` 为全局物理像素，缺省 = 主显示器整屏；
//! - 不创建任何窗口、不注册热键/托盘，可与 GUI 实例并存互不干扰；
//! - 面向 AI 的能力声明另见项目根目录 SKILL.md（何时触发、怎么调用、怎么停止）。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use xcap::Monitor;

use crate::{logger, recorder, settings};

/// 与 tauri.conf.json 的 identifier 一致（见 crate::APP_IDENTIFIER），
/// 保证 CLI 与 GUI 读写同一份设置/临时文件
const APP_DATA_SUBDIR: &str = crate::APP_IDENTIFIER;

const USAGE: &str = "\
light-capture CLI（无头模式，供脚本 / AI 调用）

用法：
  light-capture screenshot --out <path.png> [--region x,y,w,h]
  light-capture record --seconds <n> --out <path.mp4|.gif> [--region x,y,w,h] [--window <title>] [--mic] [--sys-audio] [--fps <n>] [--scale <w>] [--crf <n>] [--stop-file <path>]
  light-capture record --camera --seconds <n> --out <path.mp4|.gif> [--mic] [--sys-audio]
  light-capture analyze --in <video.mp4> [--frames <n>] [--sheet <path>] [--no-scenes]
  light-capture doctor        环境自检：FFmpeg 是否可用、版本、路径、安装建议
  light-capture list-windows  列出可见窗口（标题 + 坐标），用于挑选录制目标
  light-capture help          人类可读帮助
  light-capture help --json   机器可读能力声明（供 AI Agent 发现与调用）

参数：
  --out         输出文件路径（必填）
  --in          待分析的视频文件（analyze 必填）
  --seconds     录制时长（秒），0.2 ~ 3600；给了 --stop-file 时可省略（默认上限 3600）
  --stop-file   录制期间出现该文件即停止录制（适合 AI 随时喊停；停止后该文件被自动删除）
  --region      全局物理像素 x,y,w,h；缺省 = 主显示器整屏
  --window      按窗口标题录制（子串匹配、不区分大小写）；与 --region 互斥
  --camera      录制摄像头（dshow 首个视频设备）而非屏幕；此时 --region 无效
  --mic         录制麦克风
  --sys-audio   录制系统声音
  --fps         输出帧率 1~60（默认 30）；回看交互 15 通常足够
  --scale       输出宽度上限（像素，默认原始分辨率）；高度按比例
  --crf         x264 画质 0~51，越小越清晰（默认 23）；28 可明显减小体积
  --frames      抽取关键帧的数量（1~120）；平铺在整段时长上
  --frames-dir  关键帧输出目录（默认：视频同目录下 <文件名>.frames）
  --sheet       额外输出一张拼图（多帧平铺成单图，适合一次性喂给多模态模型）
  --cols        拼图列数（默认 4）
  --scenes      输出场景切换点与时间轴分段（analyze 默认开启；record 需显式加）
  --no-scenes   跳过场景检测（analyze 用；长视频可省时间）
  --scene-threshold  场景切换灵敏度 0.05~0.95（默认 0.30，越大越只保留明显变化）
  --no-cursor   不把鼠标指针画进画面（默认会画，AI 才能看到点在哪）
  --no-glow     不绘制指针轮廓微光（默认绘制，便于回看时定位）

输出：stdout 一行 JSON，成功 {\"ok\":true,...}，失败 {\"ok\":false,\"error\":...}；退出码 0/1
";

/// 一次 CLI 任务
enum Job {
    Screenshot {
        out: String,
        region: Option<(i32, i32, u32, u32)>,
    },
    Record {
        out: String,
        /// 录制时长上限（秒）；给了 --stop-file 时可省略，默认 3600
        seconds: f64,
        region: Option<(i32, i32, u32, u32)>,
        mic: bool,
        sys_audio: bool,
        /// 录制摄像头（而非屏幕区域）
        camera: bool,
        /// 出现该文件即停止录制（AI 随时喊停）
        stop_file: Option<String>,
        /// 是否把指针画进画面（默认 true；`--no-cursor` 关闭）
        cursor: bool,
        /// 是否在指针周围叠加轮廓微光（默认 true；`--no-glow` 关闭）
        cursor_glow: bool,
        /// 按窗口标题录制（与 region 互斥）
        window: Option<String>,
        /// 输出编码参数
        enc: recorder::EncOptions,
        /// 抽取关键帧数量（None = 不抽）
        frames: Option<u32>,
        /// 关键帧输出目录
        frames_dir: Option<String>,
        /// 拼图输出路径
        sheet: Option<String>,
        /// 拼图列数
        cols: u32,
        /// 是否输出场景切换点与时间轴分段
        scenes: bool,
        /// 场景切换阈值
        scene_threshold: f64,
    },
    /// 分析已有视频：时间轴 + 关键帧 + 拼图
    Analyze {
        input: String,
        frames: Option<u32>,
        frames_dir: Option<String>,
        sheet: Option<String>,
        cols: u32,
        scale: Option<u32>,
        scenes: bool,
        scene_threshold: f64,
    },
    /// 环境自检：FFmpeg 定位与版本
    Doctor,
    /// 列出可见窗口，供 Agent 挑选录制目标
    ListWindows,
    /// json=true 输出机器可读能力声明
    Help { json: bool },
}

/// 是否走 CLI 分支（首参是子命令或帮助），否则启动 GUI
pub fn wants_cli(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some(
            "screenshot"
                | "record"
                | "analyze"
                | "doctor"
                | "list-windows"
                | "help"
                | "--help"
                | "-h"
        )
    )
}

/// CLI 入口：返回进程退出码（0 成功 / 1 失败）
pub fn run(args: &[String]) -> i32 {
    // stdout 契约：只有结果那一行 JSON / 帮助文本，日志一律只进文件。
    // 子系统为 console（见 main.rs 文件头说明），标准句柄天然继承——
    // 管道捕获（Agent 标配）、-RedirectStandardOutput 文件、交互控制台
    // 三种方式都能拿到输出，无需 AttachConsole。logger 曾写 stdout 的
    // 杂音仍要关掉。
    logger::set_stdout_echo(false);

    let job = match parse(args) {
        Ok(j) => j,
        Err(e) => {
            emit_err(&e);
            return 1;
        }
    };
    if let Job::Help { json } = job {
        let mut out = std::io::stdout();
        if json {
            let _ = writeln!(out, "{}", declaration());
        } else {
            let _ = writeln!(out, "{USAGE}");
        }
        return 0;
    }
    init_runtime();
    match execute(job) {
        Ok(v) => {
            emit(&v);
            0
        }
        Err(e) => {
            crate::lclog!("!! CLI 执行失败：{e}");
            emit_err(&e);
            1
        }
    }
}

/// 机器可读能力声明（`help --json`）：供 AI Agent 发现本工具的能力、
/// 触发时机与调用方式。与 SKILL.md 同源信息，此处以 JSON 形式给程序读。
fn declaration() -> serde_json::Value {
    json!({
        "tool": "light-capture",
        "version": env!("CARGO_PKG_VERSION"),
        "platform": "windows",
        "summary": "轻量屏幕截图与录屏（无头 CLI，零常驻，输出文件 + 一行 JSON）",
        "when_to_use": [
            "需要截取屏幕/窗口区域并保存为图片时",
            "需要录制屏幕、系统声音、麦克风或摄像头时",
            "用户要求「录屏」「截图」「录制这段操作」等",
        ],
        "commands": [
            {
                "name": "screenshot",
                "usage": "light-capture screenshot --out <path.png> [--region x,y,w,h]",
                "description": "截取屏幕指定区域（缺省主屏整屏）保存为 PNG",
                "required": ["--out"],
                "optional": ["--region"],
            },
            {
                "name": "record",
                "usage": "light-capture record [--seconds <n>] --out <path.mp4|.gif> [--region x,y,w,h] [--window <title>] [--camera] [--mic] [--sys-audio] [--stop-file <path>] [--fps <n>] [--scale <w>] [--crf <n>] [--no-cursor] [--no-glow]",
                "description": "录制屏幕（或 --camera 摄像头）为 MP4/GIF；--seconds 与 --stop-file 至少给一个。默认把系统真实指针及其轮廓微光画进画面",
                "required": ["--out"],
                "optional": ["--seconds", "--region", "--window", "--camera", "--mic", "--sys-audio", "--stop-file", "--fps", "--scale", "--crf", "--no-cursor", "--no-glow", "--frames", "--frames-dir", "--sheet", "--cols", "--scenes", "--scene-threshold"],
                "stop": "录制期间创建 --stop-file 指定的文件即停止；停止后该文件自动删除",
            },
            {
                "name": "analyze",
                "usage": "light-capture analyze --in <video.mp4> [--frames <n>] [--sheet <path>] [--cols <n>] [--no-scenes] [--scene-threshold <f>] [--scale <w>]",
                "description": "分析已有视频：返回时间轴（时长/分辨率/帧率/场景切换点/分段），可按需抽取关键帧与生成拼图。视频用于看流程与节奏，关键帧用于看 UI 细节",
                "required": ["--in"],
                "optional": ["--frames", "--frames-dir", "--sheet", "--cols", "--scenes", "--no-scenes", "--scene-threshold", "--scale"],
            },
            {
                "name": "doctor",
                "usage": "light-capture doctor",
                "description": "环境自检：FFmpeg 是否可用、版本、解析到的路径与来源；缺失时给出可直接复制的安装命令。建议在首次录制或 analyze 之前调用一次",
                "required": [],
                "optional": [],
            },
            {
                "name": "list-windows",
                "usage": "light-capture list-windows",
                "description": "列出当前可见窗口的标题、进程名与物理像素坐标（最上层在前），配合 record --window 使用",
                "required": [],
                "optional": [],
            },
            {
                "name": "help",
                "usage": "light-capture help [--json]",
                "description": "人类可读帮助 / 机器可读能力声明（本文件）",
                "required": [],
                "optional": ["--json"],
            },
        ],
        "output": {
            "format": "stdout 单行 JSON",
            "success": {"ok": true},
            "failure": {"ok": false, "error": "原因"},
            "exit_code": {"0": "成功", "1": "失败"},
        },
        "examples": [
            "light-capture screenshot --out D:/shots/a.png --region 0,0,800,600",
            "light-capture list-windows",
            "light-capture record --window \"Visual Studio Code\" --seconds 20 --out D:/rec/demo.mp4",
            "light-capture record --seconds 10 --out D:/rec/demo.mp4 --mic --sys-audio",
            "light-capture record --seconds 30 --out D:/rec/light.mp4 --fps 15 --scale 1280 --crf 28",
            "light-capture record --stop-file D:/rec/stop.flag --out D:/rec/demo.mp4",
            "light-capture analyze --in D:/rec/demo.mp4 --frames 6 --sheet D:/rec/demo_sheet.jpg",
            "light-capture doctor",
        ],
        "constraints": [
            "Windows 专有（WGC 屏幕采集 / WASAPI 系统声音）",
            "不建窗口、不注册热键托盘，可与 GUI 版并存",
            "零常驻：进程退出即释放全部资源",
        ],
        "docs": "项目根目录 SKILL.md（完整能力声明与触发指引）",
    })
}

/// 与 GUI 相同的日志/设置初始化：日志走 crate::log_dir，设置读用户现有配置（音频增益等）。
/// 顺带清理异常退出遗留的临时录制文件——CLI 崩溃时不会走到收尾逻辑，
/// 不清理的话这些文件会一直占着磁盘。
fn init_runtime() {
    let data = app_data_dir();
    let _ = std::fs::create_dir_all(&data);
    logger::init(&crate::log_dir(&data));
    settings::init(&data);
    recorder::cleanup_stale_temps(&recordings_dir(), Duration::from_secs(6 * 3600));
}

/// 应用数据目录：与 Tauri 的 app_local_data_dir 一致（%LOCALAPPDATA%/<identifier>）
fn app_data_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|v| PathBuf::from(v).join(APP_DATA_SUBDIR))
        .unwrap_or_else(|_| std::env::temp_dir().join("light-capture"))
}

fn recordings_dir() -> PathBuf {
    app_data_dir().join("recordings")
}

fn parse(args: &[String]) -> Result<Job, String> {
    let Some(cmd) = args.first() else {
        return Err(USAGE.to_string());
    };
    if matches!(cmd.as_str(), "help" | "--help" | "-h") {
        return Ok(Job::Help {
            json: args[1..].iter().any(|a| a == "--json"),
        });
    }
    let rest = &args[1..];
    let mut out: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut region: Option<(i32, i32, u32, u32)> = None;
    let mut mic = false;
    let mut sys_audio = false;
    let mut camera = false;
    let mut stop_file: Option<String> = None;
    let mut cursor = true;
    let mut cursor_glow = true;
    let mut window: Option<String> = None;
    let mut enc = recorder::EncOptions::default();
    let mut input: Option<String> = None;
    let mut frames: Option<u32> = None;
    let mut frames_dir: Option<String> = None;
    let mut sheet: Option<String> = None;
    let mut cols: u32 = 4;
    // 是否显式指定过场景检测开关（analyze 默认开启，record 默认关闭）
    let mut scenes_flag: Option<bool> = None;
    let mut scene_threshold: f64 = 0.30;
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "--out" => out = Some(need_value(rest, &mut i, "--out")?),
            "--in" => input = Some(need_value(rest, &mut i, "--in")?),
            "--stop-file" => stop_file = Some(need_value(rest, &mut i, "--stop-file")?),
            "--window" => window = Some(need_value(rest, &mut i, "--window")?),
            "--frames" => {
                let v = need_value(rest, &mut i, "--frames")?;
                frames = Some(v.parse().map_err(|_| format!("--frames 不是整数：{v}"))?);
            }
            "--frames-dir" => frames_dir = Some(need_value(rest, &mut i, "--frames-dir")?),
            "--sheet" => sheet = Some(need_value(rest, &mut i, "--sheet")?),
            "--cols" => {
                let v = need_value(rest, &mut i, "--cols")?;
                cols = v.parse().map_err(|_| format!("--cols 不是整数：{v}"))?;
            }
            "--scenes" => scenes_flag = Some(true),
            "--no-scenes" => scenes_flag = Some(false),
            "--scene-threshold" => {
                let v = need_value(rest, &mut i, "--scene-threshold")?;
                scene_threshold = v
                    .parse()
                    .map_err(|_| format!("--scene-threshold 不是数字：{v}"))?;
            }
            "--fps" => {
                let v = need_value(rest, &mut i, "--fps")?;
                enc.fps = v.parse().map_err(|_| format!("--fps 不是整数：{v}"))?;
            }
            "--scale" => {
                let v = need_value(rest, &mut i, "--scale")?;
                enc.max_width = Some(v.parse().map_err(|_| format!("--scale 不是整数：{v}"))?);
            }
            "--crf" => {
                let v = need_value(rest, &mut i, "--crf")?;
                enc.crf = v.parse().map_err(|_| format!("--crf 不是整数：{v}"))?;
            }
            "--seconds" => {
                let v = need_value(rest, &mut i, "--seconds")?;
                let s: f64 = v.parse().map_err(|_| format!("--seconds 不是数字：{v}"))?;
                if !(0.2..=3600.0).contains(&s) {
                    return Err("--seconds 需在 0.2 ~ 3600 之间".into());
                }
                seconds = Some(s);
            }
            "--region" => {
                let v = need_value(rest, &mut i, "--region")?;
                region = Some(parse_region(&v)?);
            }
            "--mic" => mic = true,
            "--sys-audio" => sys_audio = true,
            "--camera" => camera = true,
            "--no-cursor" => cursor = false,
            "--no-glow" => cursor_glow = false,
            "--help" | "-h" => return Ok(Job::Help { json: false }),
            other => return Err(format!("未知参数：{other}\n{USAGE}")),
        }
        i += 1;
    }

    match cmd.as_str() {
        "doctor" => {
            let noisy = out.is_some()
                || seconds.is_some()
                || region.is_some()
                || window.is_some()
                || input.is_some()
                || mic
                || sys_audio
                || camera
                || stop_file.is_some();
            if noisy {
                return Err("doctor 不接受其它参数".into());
            }
            Ok(Job::Doctor)
        }
        "analyze" => {
            if out.is_some() || seconds.is_some() || region.is_some() || window.is_some() {
                return Err("analyze 只接受 --in 与抽帧相关参数".into());
            }
            let input = input.ok_or_else(|| format!("analyze 缺少 --in\n{USAGE}"))?;
            if !Path::new(&input).exists() {
                return Err(format!("视频文件不存在：{input}"));
            }
            let frames = frames
                .map(|n| {
                    if (1..=120).contains(&n) {
                        Ok(n)
                    } else {
                        Err(format!("--frames 需在 1 ~ 120 之间：{n}"))
                    }
                })
                .transpose()?;
            if !(0.05..=0.95).contains(&scene_threshold) {
                return Err("--scene-threshold 需在 0.05 ~ 0.95 之间".into());
            }
            if !(1..=12).contains(&cols) {
                return Err("--cols 需在 1 ~ 12 之间".into());
            }
            Ok(Job::Analyze {
                input,
                frames,
                frames_dir,
                sheet,
                cols,
                scale: enc.max_width,
                // analyze 的主角就是时间轴，默认开启场景检测；可用 --no-scenes 跳过
                scenes: scenes_flag.unwrap_or(true),
                scene_threshold,
            })
        }
        "list-windows" => {
            if out.is_some()
                || seconds.is_some()
                || region.is_some()
                || window.is_some()
                || mic
                || sys_audio
                || camera
                || stop_file.is_some()
                || !cursor
                || !cursor_glow
            {
                return Err("list-windows 不接受其它参数".into());
            }
            Ok(Job::ListWindows)
        }
        "screenshot" => {
            if seconds.is_some()
                || mic
                || sys_audio
                || camera
                || stop_file.is_some()
                || !cursor
                || !cursor_glow
                || window.is_some()
            {
                return Err(
                    "screenshot 不支持 --seconds / --mic / --sys-audio / --camera / --stop-file / --window / --no-cursor / --no-glow"
                        .into(),
                );
            }
            let out = out.ok_or_else(|| format!("screenshot 缺少 --out\n{USAGE}"))?;
            Ok(Job::Screenshot { out, region })
        }
        "record" => {
            if camera && region.is_some() {
                return Err("--camera 不支持 --region（摄像头没有选区概念）".into());
            }
            if camera && window.is_some() {
                return Err("--camera 不支持 --window（摄像头不是窗口）".into());
            }
            if window.is_some() && region.is_some() {
                return Err("--window 与 --region 只能二选一".into());
            }
            let enc = enc.validate()?;
            if !(0.05..=0.95).contains(&scene_threshold) {
                return Err("--scene-threshold 需在 0.05 ~ 0.95 之间".into());
            }
            if !(1..=12).contains(&cols) {
                return Err("--cols 需在 1 ~ 12 之间".into());
            }
            if let Some(n) = frames {
                if !(1..=120).contains(&n) {
                    return Err(format!("--frames 需在 1 ~ 120 之间：{n}"));
                }
            }
            let out = out.ok_or_else(|| format!("record 缺少 --out\n{USAGE}"))?;
            // --seconds 与 --stop-file 至少给一个：要么定时，要么随时喊停
            let seconds = match (seconds, stop_file.is_some()) {
                (None, false) => {
                    return Err(format!(
                        "record 需要 --seconds 或 --stop-file 至少一个\n{USAGE}"
                    ));
                }
                (None, true) => 3600.0, // 只给了停止文件 = 以上限兜底，实际由停止文件结束
                (Some(s), _) => s,
            };
            Ok(Job::Record {
                out,
                seconds,
                region,
                mic,
                sys_audio,
                camera,
                stop_file,
                cursor,
                cursor_glow,
                window,
                enc,
                frames,
                frames_dir,
                sheet,
                cols,
                // 录制后分析按需开启：只有显式 --scenes 才跑场景检测
                scenes: scenes_flag.unwrap_or(false),
                scene_threshold,
            })
        }
        other => Err(format!("未知命令：{other}\n{USAGE}")),
    }
}

/// 取带值参数的下一个参数值；i 指向标志位本身，函数内前移一位消费掉值
fn need_value(rest: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    match rest.get(*i + 1) {
        Some(v) if !v.starts_with("--") => {
            *i += 1;
            Ok(v.clone())
        }
        _ => Err(format!("{flag} 缺少参数值\n{USAGE}")),
    }
}

fn parse_region(v: &str) -> Result<(i32, i32, u32, u32), String> {
    let parts: Vec<&str> = v.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!("--region 格式应为 x,y,w,h：{v}"));
    }
    let x: i32 = parts[0]
        .parse()
        .map_err(|_| format!("--region 的 x 无效：{v}"))?;
    let y: i32 = parts[1]
        .parse()
        .map_err(|_| format!("--region 的 y 无效：{v}"))?;
    let w: u32 = parts[2]
        .parse()
        .map_err(|_| format!("--region 的 w 无效：{v}"))?;
    let h: u32 = parts[3]
        .parse()
        .map_err(|_| format!("--region 的 h 无效：{v}"))?;
    if w < 2 || h < 2 {
        return Err("--region 的宽高至少 2 像素".into());
    }
    Ok((x, y, w, h))
}

/// 缺省区域 = 主显示器整屏（Windows 虚拟桌面坐标里主屏左上角为 (0,0)）
fn primary_region() -> Result<(i32, i32, u32, u32), String> {
    let mon = Monitor::from_point(0, 0).map_err(|e| format!("获取主显示器失败：{e:?}"))?;
    let x = mon.x().map_err(|e| e.to_string())?;
    let y = mon.y().map_err(|e| e.to_string())?;
    let w = mon.width().map_err(|e| e.to_string())?;
    let h = mon.height().map_err(|e| e.to_string())?;
    Ok((x, y, w, h))
}

fn resolve_region(region: Option<(i32, i32, u32, u32)>) -> Result<(i32, i32, u32, u32), String> {
    region.map(Ok).unwrap_or_else(primary_region)
}

/// 按标题子串查找窗口（不区分大小写）。多个命中时优先精确匹配，其次取 Z 序最上层。
fn find_window(title: &str) -> Result<recorder::WindowInfo, String> {
    let all = recorder::list_windows()?;
    let needle = title.trim().to_lowercase();
    if needle.is_empty() {
        return Err("--window 需要一个非空的窗口标题".into());
    }
    let exact = all
        .iter()
        .find(|w| w.title.trim().to_lowercase() == needle)
        .cloned();
    let matched = exact.or_else(|| {
        all.iter()
            .find(|w| w.title.to_lowercase().contains(&needle))
            .cloned()
    });
    matched.ok_or_else(|| {
        let names: Vec<String> = all.iter().take(12).map(|w| w.title.clone()).collect();
        format!(
            "没有找到标题包含「{title}」的窗口。当前可见窗口：{}",
            if names.is_empty() {
                "（无）".to_string()
            } else {
                names.join(" / ")
            }
        )
    })
}

/// 分析一段视频：时间轴（时长/分辨率/帧率/场景切换/分段）+ 可选关键帧与拼图。
///
/// Agent 的典型用法：先读 timeline 决定看哪几段，再让它按需抽帧；
/// 或者一次 `--frames 6 --sheet out.jpg` 拿到全量视觉材料。
#[allow(clippy::too_many_arguments)]
fn analyze_video(
    input: &str,
    frames: Option<u32>,
    frames_dir: Option<&str>,
    sheet: Option<&str>,
    cols: u32,
    scale: Option<u32>,
    want_scenes: bool,
    scene_threshold: f64,
) -> Result<serde_json::Value, String> {
    let info = crate::frames::probe(input)?;
    crate::lclog!(
        "视频分析：{input} {:.2}s {}x{} {:.2}fps",
        info.duration,
        info.width,
        info.height,
        info.fps
    );

    let cuts = if want_scenes {
        // 场景检测失败不该让整个分析失败：时间轴本身仍然有价值
        match crate::frames::scene_cuts(input, scene_threshold) {
            Ok(c) => c,
            Err(e) => {
                crate::lclog!("!! 场景检测失败（忽略）：{e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let segments = crate::frames::segments_from_cuts(info.duration, &cuts);

    let mut timeline = json!({
        "duration": round2(info.duration),
        "width": info.width,
        "height": info.height,
        "fps": round2(info.fps),
        "scene_cuts": cuts
            .iter()
            .map(|c| json!({"t": round2(c.t), "score": round2(c.score)}))
            .collect::<Vec<_>>(),
        "segments": segments
            .iter()
            .map(|s| json!({
                "start": round2(s.start),
                "end": round2(s.end),
                "duration": round2(s.duration()),
            }))
            .collect::<Vec<_>>(),
    });

    // 关键帧：给了数量就均匀抽；没给但要求了拼图，则按拼图数量抽
    let frame_count = frames.or_else(|| sheet.map(|_| cols.max(1) * 2));
    if let Some(n) = frame_count {
        let dir = match frames_dir {
            Some(d) => PathBuf::from(d),
            None => default_frames_dir(input),
        };
        let times = crate::frames::sample_times(info.duration, n);
        let ext = "jpg";
        let saved = crate::frames::extract_frames(input, &dir, &times, scale, ext)?;
        let frame_files: Vec<String> = saved
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        crate::lclog!("已抽取 {} 张关键帧 → {}", frame_files.len(), dir.display());
        timeline["frames_dir"] = json!(dir.to_string_lossy());
        timeline["frames"] = json!(frame_files);
        timeline["frame_times"] = json!(times.iter().map(|t| round2(*t)).collect::<Vec<_>>());
    }

    if let Some(path) = sheet {
        crate::frames::contact_sheet(
            input,
            Path::new(path),
            info.duration,
            frame_count.unwrap_or(6),
            cols,
            scale,
        )?;
        crate::lclog!("已生成拼图 → {path}");
        timeline["sheet"] = json!(path);
    }

    Ok(timeline)
}

/// 默认关键帧目录：与视频同级的 `<文件名>.frames/`，避免污染用户目录。
fn default_frames_dir(video: &str) -> PathBuf {
    let p = Path::new(video);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "frames".into());
    match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(format!("{stem}.frames")),
        _ => PathBuf::from(format!("{stem}.frames")),
    }
}

/// JSON 里保留两位小数，避免输出 3.0000000000000004 这类噪声
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn execute(job: Job) -> Result<serde_json::Value, String> {
    match job {
        Job::Help { .. } => unreachable!("帮助已在 run() 提前处理"),
        Job::Doctor => {
            let report = crate::ffmpeg::diagnose();
            // doctor 是诊断命令：找不到依赖也返回 ok:true，
            // 让 Agent 能从同一条 JSON 里读到 found/error/hint，而不是靠退出码猜
            Ok(json!({
                "ok": true,
                "command": "doctor",
                "ffmpeg": report,
            }))
        }
        Job::ListWindows => {
            let windows = recorder::list_windows()?;
            Ok(json!({
                "ok": true,
                "command": "list-windows",
                "count": windows.len(),
                "windows": windows,
            }))
        }
        Job::Analyze {
            input,
            frames,
            frames_dir,
            sheet,
            cols,
            scale,
            scenes,
            scene_threshold,
        } => {
            let timeline = analyze_video(
                &input,
                frames,
                frames_dir.as_deref(),
                sheet.as_deref(),
                cols,
                scale,
                scenes,
                scene_threshold,
            )?;
            Ok(json!({
                "ok": true,
                "command": "analyze",
                "path": input,
                "timeline": timeline,
            }))
        }
        Job::Screenshot { out, region } => {
            let (px, py, pw, ph) = resolve_region(region)?;
            crate::lclog!("CLI 截图：({px},{py} {pw}x{ph}) → {out}");
            let shot = recorder::capture(px, py, pw, ph)?;
            let png = crate::decode_data_url(&shot.data_url)?;
            std::fs::write(&out, &png).map_err(|e| format!("写入 {out} 失败：{e}"))?;
            Ok(json!({
                "ok": true,
                "command": "screenshot",
                "path": out,
                "width": shot.width,
                "height": shot.height,
                "region": {"x": px, "y": py, "width": pw, "height": ph},
            }))
        }
        Job::Record {
            out,
            seconds,
            region,
            mic,
            sys_audio,
            camera,
            stop_file,
            cursor,
            cursor_glow,
            window,
            enc,
            frames,
            frames_dir,
            sheet,
            cols,
            scenes,
            scene_threshold,
        } => {
            // 按窗口录制：开录时定位一次，换算成物理像素区域
            let window_info = match &window {
                Some(title) => Some(find_window(title)?),
                None => None,
            };
            let (px, py, pw, ph) = if camera {
                (0, 0, 0, 0)
            } else if let Some(w) = &window_info {
                (w.x, w.y, w.width, w.height)
            } else {
                resolve_region(region)?
            };
            let fmt = if out.to_lowercase().ends_with(".gif") {
                "gif"
            } else {
                "mp4"
            };
            let mode = if camera { "camera" } else { "cli" };
            // 清掉上次残留的停止文件，避免刚开录就被喊停
            if let Some(sf) = &stop_file {
                let _ = std::fs::remove_file(sf);
            }
            // 输出目录不存在时直接失败，免得录完才报错丢结果
            if let Some(parent) = Path::new(&out).parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    return Err(format!("输出目录不存在：{}", parent.display()));
                }
            }
            crate::lclog!(
                "CLI 录制（{mode}）：({px},{py} {pw}x{ph}) {seconds}s → {out} mic={mic} sys={sys_audio} fps={} scale={:?} crf={} window={:?}",
                enc.fps,
                enc.max_width,
                enc.crf,
                window
            );
            let info = recorder::start(
                recordings_dir(),
                mode.into(),
                px,
                py,
                pw,
                ph,
                mic,
                sys_audio,
                cursor,
                cursor_glow,
                enc,
            )
            .map_err(|e| format!("启动录制失败：{e}"))?;
            // 等待：到时长上限，或停止文件出现（谁先到听谁的）
            let stopped = wait_for_stop(seconds, stop_file.as_deref());
            if let Some(sf) = &stop_file {
                let _ = std::fs::remove_file(sf);
            }
            let stop = recorder::stop(false).map_err(|e| format!("停止录制失败：{e}"))?;
            let src = stop.path.ok_or_else(|| "录制未产生文件".to_string())?;
            // 以实际录制时长为终点，避免补帧边界导致选段超出片长
            let end = if stop.seconds > 0.3 {
                stop.seconds
            } else {
                seconds
            };
            // CLI 录制的就是整段：MP4 直接换容器（copy），省掉一次全量重编码；
            // GIF 无法 copy，仍走转码。
            let exported = if fmt == "gif" {
                recorder::export(&src, 0.0, end, &out, fmt)
            } else {
                recorder::export_copy(&src, &out)
            };
            let _ = recorder::discard(&src);
            let path = exported?;
            let region_json = if camera {
                serde_json::Value::Null
            } else {
                json!({"x": px, "y": py, "width": pw, "height": ph})
            };
            // 录制后可选：直接给出时间轴 / 关键帧 / 拼图，省掉 Agent 再调一次 analyze
            let analysis = if frames.is_some() || sheet.is_some() || scenes {
                match analyze_video(
                    &path,
                    frames,
                    frames_dir.as_deref(),
                    sheet.as_deref(),
                    cols,
                    enc.max_width,
                    scenes,
                    scene_threshold,
                ) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        // 录制本身已成功，分析失败只降级为告警
                        crate::lclog!("!! 录制后分析失败（视频仍可用）：{e}");
                        Some(json!({"error": e}))
                    }
                }
            } else {
                None
            };
            Ok(json!({
                "ok": true,
                "command": "record",
                "camera": camera,
                "path": path,
                "format": fmt,
                "seconds": end,
                "region": region_json,
                "window": window_info
                    .as_ref()
                    .map(|w| json!({"title": w.title, "app_name": w.app_name, "x": w.x, "y": w.y, "width": w.width, "height": w.height})),
                "fps": enc.fps,
                "scale": enc.max_width,
                "crf": enc.crf,
                "mic": info.mic,
                "sys_audio": info.sys_audio,
                "audio_warnings": info.audio_warnings,
                "cursor": !camera && cursor,
                "cursor_glow": !camera && cursor && cursor_glow,
                "stopped_by": if stopped { "stop_file" } else { "duration" },
                "timeline": analysis,
            }))
        }
    }
}

/// 等到时长上限或停止文件出现（100ms 轮询）。返回是否被停止文件提前结束。
fn wait_for_stop(seconds: f64, stop_file: Option<&str>) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(seconds);
    loop {
        if let Some(sf) = stop_file {
            if Path::new(sf).exists() {
                crate::lclog!("检测到停止文件 {sf}，提前结束录制");
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn emit(v: &serde_json::Value) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{v}");
}

fn emit_err(msg: &str) {
    emit(&json!({ "ok": false, "error": msg }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wants_cli_only_for_commands() {
        assert!(wants_cli(&args(&["screenshot"])));
        assert!(wants_cli(&args(&["record", "--seconds", "3"])));
        assert!(wants_cli(&args(&["help"])));
        assert!(wants_cli(&args(&["list-windows"])));
        assert!(
            !wants_cli(&args(&["--no-such"])),
            "未知首参应交给 GUI/报错路径"
        );
        assert!(!wants_cli(&[]), "无参数 = 正常启动 GUI");
    }

    #[test]
    fn parse_screenshot_ok() {
        let j = parse(&args(&[
            "screenshot",
            "--out",
            r"D:\a.png",
            "--region",
            "0,0,800,600",
        ]))
        .expect("应解析成功");
        match j {
            Job::Screenshot { out, region } => {
                assert_eq!(out, r"D:\a.png");
                assert_eq!(region, Some((0, 0, 800, 600)));
            }
            _ => panic!("命令类型错误"),
        }
    }

    #[test]
    fn parse_record_ok_with_flags() {
        let j = parse(&args(&[
            "record",
            "--seconds",
            "12.5",
            "--out",
            "demo.gif",
            "--mic",
            "--sys-audio",
        ]))
        .expect("应解析成功");
        match j {
            Job::Record {
                out,
                seconds,
                region,
                mic,
                sys_audio,
                camera,
                stop_file,
                cursor,
                cursor_glow,
                window,
                enc,
                frames,
                sheet,
                scenes,
                ..
            } => {
                assert_eq!(out, "demo.gif");
                assert_eq!(seconds, 12.5);
                assert_eq!(region, None);
                assert!(mic && sys_audio);
                assert!(!camera, "缺省为屏幕录制");
                assert!(stop_file.is_none(), "未给 --stop-file 时为空");
                assert!(cursor && cursor_glow, "指针与光晕默认开启");
                assert!(window.is_none(), "未给 --window 时为整屏/region");
                assert!(frames.is_none() && sheet.is_none() && !scenes, "默认不分析");
                assert_eq!(enc.fps, 30, "默认帧率");
                assert_eq!(enc.crf, 23, "默认画质");
                assert!(enc.max_width.is_none(), "默认不缩放");
            }
            _ => panic!("命令类型错误"),
        }
    }

    #[test]
    fn parse_analyze_ok() {
        let tmp = std::env::temp_dir().join(format!("lc_an_{}.mp4", crate::stamp()));
        std::fs::write(&tmp, b"x").unwrap();
        let p = tmp.to_string_lossy().into_owned();
        match parse(&args(&[
            "analyze",
            "--in",
            &p,
            "--frames",
            "6",
            "--sheet",
            "out.jpg",
            "--cols",
            "3",
            "--scenes",
            "--scene-threshold",
            "0.4",
        ]))
        .expect("应解析成功")
        {
            Job::Analyze {
                input,
                frames,
                sheet,
                cols,
                scenes,
                scene_threshold,
                ..
            } => {
                assert_eq!(input, p);
                assert_eq!(frames, Some(6));
                assert_eq!(sheet.as_deref(), Some("out.jpg"));
                assert_eq!(cols, 3);
                assert!(scenes);
                assert!((scene_threshold - 0.4).abs() < 0.001);
            }
            _ => panic!("命令类型错误"),
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn doctor_accepts_no_extra_flags() {
        assert!(matches!(
            parse(&args(&["doctor"])).expect("应解析成功"),
            Job::Doctor
        ));
        assert!(parse(&args(&["doctor", "--out", "a.png"])).is_err());
    }

    #[test]
    fn analyze_rejects_bad_input() {
        // 缺 --in
        assert!(parse(&args(&["analyze"])).is_err());
        // 文件不存在
        assert!(parse(&args(&["analyze", "--in", "definitely_missing_98765.mp4"])).is_err());
        // 参数区间
        let tmp = std::env::temp_dir().join(format!("lc_an2_{}.mp4", crate::stamp()));
        std::fs::write(&tmp, b"x").unwrap();
        let p = tmp.to_string_lossy().into_owned();
        assert!(parse(&args(&["analyze", "--in", &p, "--frames", "0"])).is_err());
        assert!(parse(&args(&["analyze", "--in", &p, "--cols", "99"])).is_err());
        assert!(parse(&args(&["analyze", "--in", &p, "--scene-threshold", "2"])).is_err());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn record_accepts_analysis_flags() {
        match parse(&args(&[
            "record",
            "--seconds",
            "5",
            "--out",
            "a.mp4",
            "--frames",
            "4",
            "--sheet",
            "s.jpg",
            "--scenes",
        ]))
        .expect("应解析成功")
        {
            Job::Record {
                frames,
                sheet,
                scenes,
                ..
            } => {
                assert_eq!(frames, Some(4));
                assert_eq!(sheet.as_deref(), Some("s.jpg"));
                assert!(scenes);
            }
            _ => panic!("命令类型错误"),
        }
    }

    #[test]
    fn parse_record_window_and_encoding() {
        match parse(&args(&[
            "record",
            "--seconds",
            "20",
            "--out",
            "a.mp4",
            "--window",
            "Visual Studio Code",
            "--fps",
            "15",
            "--scale",
            "1280",
            "--crf",
            "28",
        ]))
        .expect("应解析成功")
        {
            Job::Record { window, enc, .. } => {
                assert_eq!(window.as_deref(), Some("Visual Studio Code"));
                assert_eq!(enc.fps, 15);
                assert_eq!(enc.max_width, Some(1280));
                assert_eq!(enc.crf, 28);
            }
            _ => panic!("命令类型错误"),
        }
        // --window 与 --region 互斥
        assert!(
            parse(&args(&[
                "record",
                "--seconds",
                "3",
                "--out",
                "a.mp4",
                "--window",
                "x",
                "--region",
                "0,0,10,10"
            ]))
            .is_err(),
            "两者不能同时给"
        );
        // 摄像头不能按窗口录
        assert!(parse(&args(&[
            "record",
            "--camera",
            "--seconds",
            "3",
            "--out",
            "a.mp4",
            "--window",
            "x"
        ]))
        .is_err());
        // 越界参数应在解析阶段就报错
        assert!(parse(&args(&[
            "record",
            "--seconds",
            "3",
            "--out",
            "a.mp4",
            "--fps",
            "0"
        ]))
        .is_err());
        assert!(parse(&args(&[
            "record",
            "--seconds",
            "3",
            "--out",
            "a.mp4",
            "--crf",
            "60"
        ]))
        .is_err());
    }

    #[test]
    fn list_windows_accepts_no_extra_flags() {
        assert!(matches!(
            parse(&args(&["list-windows"])).expect("应解析成功"),
            Job::ListWindows
        ));
        assert!(parse(&args(&["list-windows", "--out", "a.png"])).is_err());
    }

    #[test]
    fn parse_record_cursor_flags() {
        match parse(&args(&[
            "record",
            "--seconds",
            "3",
            "--out",
            "a.mp4",
            "--no-cursor",
        ]))
        .expect("应解析成功")
        {
            Job::Record {
                cursor,
                cursor_glow,
                ..
            } => {
                assert!(!cursor, "--no-cursor 应关闭指针");
                assert!(cursor_glow, "光晕开关独立");
            }
            _ => panic!("命令类型错误"),
        }
        match parse(&args(&[
            "record",
            "--seconds",
            "3",
            "--out",
            "a.mp4",
            "--no-glow",
        ]))
        .expect("应解析成功")
        {
            Job::Record {
                cursor,
                cursor_glow,
                ..
            } => {
                assert!(cursor);
                assert!(!cursor_glow, "--no-glow 应关闭光晕");
            }
            _ => panic!("命令类型错误"),
        }
        assert!(
            parse(&args(&["screenshot", "--out", "a.png", "--no-glow"])).is_err(),
            "screenshot 应拒绝 --no-glow"
        );
    }

    #[test]
    fn parse_record_with_stop_file() {
        // 只给停止文件、不给时长：以上限兜底，由停止文件实际结束
        let j = parse(&args(&[
            "record",
            "--stop-file",
            r"D:\tmp\stop.flag",
            "--out",
            "demo.mp4",
        ]))
        .expect("只给 --stop-file 应解析成功");
        match j {
            Job::Record {
                seconds, stop_file, ..
            } => {
                assert_eq!(stop_file.as_deref(), Some(r"D:\tmp\stop.flag"));
                assert_eq!(seconds, 3600.0, "缺省上限兜底");
            }
            _ => panic!("命令类型错误"),
        }
        // 时长 + 停止文件可共存（谁先到听谁的）
        match parse(&args(&[
            "record",
            "--seconds",
            "60",
            "--stop-file",
            "stop.flag",
            "--out",
            "a.mp4",
        ]))
        .expect("两者共存应成功")
        {
            Job::Record {
                seconds, stop_file, ..
            } => {
                assert_eq!(seconds, 60.0);
                assert!(stop_file.is_some());
            }
            _ => panic!("命令类型错误"),
        }
        // screenshot 不支持 --stop-file
        assert!(
            parse(&args(&["screenshot", "--out", "a.png", "--stop-file", "f"])).is_err(),
            "screenshot 应拒绝 --stop-file"
        );
        // 两者都不给应报错
        assert!(
            parse(&args(&["record", "--out", "a.mp4"])).is_err(),
            "record 需要 --seconds 或 --stop-file 至少一个"
        );
    }

    #[test]
    fn help_json_flag_parsed() {
        match parse(&args(&["help", "--json"])).expect("应解析成功") {
            Job::Help { json } => assert!(json),
            _ => panic!("命令类型错误"),
        }
        match parse(&args(&["help"])).expect("应解析成功") {
            Job::Help { json } => assert!(!json),
            _ => panic!("命令类型错误"),
        }
    }

    #[test]
    fn declaration_is_machine_readable() {
        let d = declaration();
        assert_eq!(d["tool"], "light-capture");
        assert_eq!(d["platform"], "windows");
        // 各子命令都在
        let names: Vec<&str> = d["commands"]
            .as_array()
            .expect("commands 应为数组")
            .iter()
            .map(|c| c["name"].as_str().expect("name 应为字符串"))
            .collect();
        assert_eq!(
            names,
            vec![
                "screenshot",
                "record",
                "analyze",
                "doctor",
                "list-windows",
                "help"
            ]
        );
        // record 的声明里必须能查到新老关键参数
        let record = &d["commands"][1];
        for flag in [
            "--stop-file",
            "--window",
            "--fps",
            "--scale",
            "--crf",
            "--no-cursor",
        ] {
            assert!(
                record["optional"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v == flag),
                "record 声明需包含 {flag}"
            );
        }
        assert!(record["stop"].as_str().is_some(), "需说明如何停止");
        // 输出契约可机读
        assert_eq!(d["output"]["exit_code"]["0"], "成功");
        // 能序列化成单行 JSON（stdout 输出的前提）
        let s = d.to_string();
        assert!(!s.contains('\n'), "声明应能单行输出");
        assert!(serde_json::from_str::<serde_json::Value>(&s).is_ok());
    }

    #[test]
    fn wait_for_stop_timeout_without_file() {
        // 没有停止文件：应等到时长自然结束（取下限 0.2s 验证）
        let t = std::time::Instant::now();
        let stopped = wait_for_stop(0.2, None);
        assert!(!stopped, "无停止文件应走满时长");
        assert!(t.elapsed() >= Duration::from_millis(190), "应等满时长");
    }

    #[test]
    fn wait_for_stop_detects_existing_file() {
        let sf = std::env::temp_dir().join(format!("lc_stop_test_{}.flag", crate::stamp()));
        std::fs::write(&sf, b"x").unwrap();
        let t = std::time::Instant::now();
        let stopped = wait_for_stop(5.0, sf.to_str());
        assert!(stopped, "停止文件存在应立即结束");
        assert!(t.elapsed() < Duration::from_secs(1), "不应等满 5 秒");
        let _ = std::fs::remove_file(&sf);
    }

    #[test]
    fn wait_for_stop_detects_file_created_later() {
        // 模拟 AI 中途喊停：另线程 300ms 后创建停止文件
        let sf = std::env::temp_dir().join(format!("lc_stop_later_{}.flag", crate::stamp()));
        let _ = std::fs::remove_file(&sf);
        let sf2 = sf.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = std::fs::write(&sf2, b"stop");
        });
        let t = std::time::Instant::now();
        let stopped = wait_for_stop(10.0, sf.to_str());
        assert!(stopped, "中途创建的停止文件应生效");
        let el = t.elapsed();
        assert!(
            el >= Duration::from_millis(280) && el < Duration::from_secs(2),
            "应在文件创建附近停止，实际 {el:?}"
        );
        let _ = std::fs::remove_file(&sf);
    }

    #[test]
    fn parse_record_camera() {
        let j = parse(&args(&[
            "record",
            "--camera",
            "--seconds",
            "5",
            "--out",
            "cam.mp4",
            "--mic",
        ]))
        .expect("--camera 应解析成功");
        match j {
            Job::Record {
                camera,
                region,
                mic,
                ..
            } => {
                assert!(camera);
                assert_eq!(region, None);
                assert!(mic);
            }
            _ => panic!("命令类型错误"),
        }
        assert!(
            parse(&args(&[
                "record",
                "--camera",
                "--seconds",
                "5",
                "--out",
                "a.mp4",
                "--region",
                "0,0,100,100"
            ]))
            .is_err(),
            "--camera 与 --region 互斥"
        );
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(parse(&args(&["screenshot"])).is_err(), "缺 --out 应报错");
        assert!(
            parse(&args(&["record", "--out", "a.mp4"])).is_err(),
            "缺 --seconds 应报错"
        );
        assert!(
            parse(&args(&["record", "--seconds", "0", "--out", "a.mp4"])).is_err(),
            "时长下限应拦截"
        );
        assert!(
            parse(&args(&[
                "screenshot",
                "--out",
                "a.png",
                "--region",
                "1,2,3"
            ]))
            .is_err(),
            "region 三段应报错"
        );
        assert!(
            parse(&args(&[
                "screenshot",
                "--out",
                "a.png",
                "--region",
                "0,0,1,1"
            ]))
            .is_err(),
            "region 过小应报错"
        );
        assert!(parse(&args(&["nope"])).is_err(), "未知命令应报错");
    }

    #[test]
    fn parse_region_values() {
        assert_eq!(
            parse_region(" 10 , 20 , 300 , 400 ").unwrap(),
            (10, 20, 300, 400)
        );
        assert_eq!(
            parse_region("-10,-20,300,400").unwrap(),
            (-10, -20, 300, 400)
        );
    }
}
