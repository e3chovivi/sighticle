//! CLI 无头模式：给脚本 / AI 编排调用的截图与录屏入口。
//!
//! 用法（详见 `light-capture help`；机器可读能力声明 `light-capture help --json`）：
//!   light-capture screenshot --out shot.png [--region x,y,w,h]
//!   light-capture record --seconds 10 --out demo.mp4 [--region x,y,w,h] [--mic] [--sys-audio]
//!   light-capture record --camera --seconds 5 --out cam.mp4 [--mic]
//!   light-capture record --stop-file stop.flag --out demo.mp4   # 随时停止（创建文件即停）
//!
//! 约定：
//! - stdout 只输出一行 JSON（机器可读），详细过程照常写 D 盘日志文件；
//! - 成功退出码 0、失败 1；`--region` 为全局物理像素，缺省 = 主显示器整屏；
//! - 不创建任何窗口、不注册热键/托盘，可与 GUI 实例并存互不干扰；
//! - 面向 AI 的能力声明另见项目根目录 SKILL.md（何时触发、怎么调用、怎么停止）。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use xcap::Monitor;

use crate::{logger, recorder, settings};

/// 与 tauri.conf.json 的 identifier 一致，保证 CLI 与 GUI 读写同一份设置/临时文件
const APP_DATA_SUBDIR: &str = "com.tauri-app.light-capture";

const USAGE: &str = "\
light-capture CLI（无头模式，供脚本 / AI 调用）

用法：
  light-capture screenshot --out <path.png> [--region x,y,w,h]
  light-capture record --seconds <n> --out <path.mp4|.gif> [--region x,y,w,h] [--mic] [--sys-audio] [--stop-file <path>]
  light-capture record --camera --seconds <n> --out <path.mp4|.gif> [--mic] [--sys-audio] [--stop-file <path>]
  light-capture help            人类可读帮助
  light-capture help --json     机器可读能力声明（供 AI Agent 发现与调用）

参数：
  --out         输出文件路径（必填）
  --seconds     录制时长（秒），0.2 ~ 3600；给了 --stop-file 时可省略（默认上限 3600）
  --stop-file   录制期间出现该文件即停止录制（适合 AI 随时喊停；停止后该文件被自动删除）
  --region      全局物理像素 x,y,w,h；缺省 = 主显示器整屏
  --camera      录制摄像头（dshow 首个视频设备）而非屏幕；此时 --region 无效
  --mic         录制麦克风
  --sys-audio   录制系统声音

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
    },
    /// json=true 输出机器可读能力声明
    Help {
        json: bool,
    },
}

/// 是否走 CLI 分支（首参是子命令或帮助），否则启动 GUI
pub fn wants_cli(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("screenshot" | "record" | "help" | "--help" | "-h")
    )
}

/// CLI 入口：返回进程退出码（0 成功 / 1 失败）
pub fn run(args: &[String]) -> i32 {
    // stdout 契约：只有结果那一行 JSON / 帮助文本，日志一律只进文件
    logger::set_stdout_echo(false);
    // 释放构建是 windows_subsystem="windows"，默认不带控制台；
    // 附到父进程控制台，让 `light-capture ...` 在终端里能直接看到输出
    //（AI 以管道捕获 stdout 时本来就有句柄，不受影响）。
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }

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
                "usage": "light-capture record [--seconds <n>] --out <path.mp4|.gif> [--region x,y,w,h] [--camera] [--mic] [--sys-audio] [--stop-file <path>]",
                "description": "录制屏幕（或 --camera 摄像头）为 MP4/GIF；--seconds 与 --stop-file 至少给一个",
                "required": ["--out"],
                "optional": ["--seconds", "--region", "--camera", "--mic", "--sys-audio", "--stop-file"],
                "stop": "录制期间创建 --stop-file 指定的文件即停止；停止后该文件自动删除",
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
            "light-capture record --seconds 10 --out D:/rec/demo.mp4 --mic --sys-audio",
            "light-capture record --stop-file D:/rec/stop.flag --out D:/rec/demo.mp4",
        ],
        "constraints": [
            "Windows 专有（WGC 屏幕采集 / WASAPI 系统声音）",
            "不建窗口、不注册热键托盘，可与 GUI 版并存",
            "零常驻：进程退出即释放全部资源",
        ],
        "docs": "项目根目录 SKILL.md（完整能力声明与触发指引）",
    })
}

/// 与 GUI 相同的日志/设置初始化：日志留 D 盘，设置读用户现有配置（音频增益等）
fn init_runtime() {
    let data = app_data_dir();
    let _ = std::fs::create_dir_all(&data);
    logger::init(&logger::prefer_dir(Path::new(crate::LOG_ROOT_ON_D), &data));
    settings::init(&data);
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
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "--out" => out = Some(need_value(rest, &mut i, "--out")?),
            "--stop-file" => stop_file = Some(need_value(rest, &mut i, "--stop-file")?),
            "--seconds" => {
                let v = need_value(rest, &mut i, "--seconds")?;
                let s: f64 = v
                    .parse()
                    .map_err(|_| format!("--seconds 不是数字：{v}"))?;
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
            "--help" | "-h" => return Ok(Job::Help { json: false }),
            other => return Err(format!("未知参数：{other}\n{USAGE}")),
        }
        i += 1;
    }

    match cmd.as_str() {
        "screenshot" => {
            if seconds.is_some() || mic || sys_audio || camera || stop_file.is_some() {
                return Err(
                    "screenshot 不支持 --seconds / --mic / --sys-audio / --camera / --stop-file"
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
            let out = out.ok_or_else(|| format!("record 缺少 --out\n{USAGE}"))?;
            // --seconds 与 --stop-file 至少给一个：要么定时，要么随时喊停
            let seconds = match (seconds, stop_file.is_some()) {
                (None, false) => {
                    return Err(format!("record 需要 --seconds 或 --stop-file 至少一个\n{USAGE}"));
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
    let x: i32 = parts[0].parse().map_err(|_| format!("--region 的 x 无效：{v}"))?;
    let y: i32 = parts[1].parse().map_err(|_| format!("--region 的 y 无效：{v}"))?;
    let w: u32 = parts[2].parse().map_err(|_| format!("--region 的 w 无效：{v}"))?;
    let h: u32 = parts[3].parse().map_err(|_| format!("--region 的 h 无效：{v}"))?;
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

fn execute(job: Job) -> Result<serde_json::Value, String> {
    match job {
        Job::Help { .. } => unreachable!("帮助已在 run() 提前处理"),
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
        } => {
            let (px, py, pw, ph) = if camera {
                (0, 0, 0, 0)
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
            crate::lclog!(
                "CLI 录制（{mode}）：({px},{py} {pw}x{ph}) {seconds}s → {out} mic={mic} sys={sys_audio} stop={stop_file:?}"
            );
            let info =
                recorder::start(recordings_dir(), mode.into(), px, py, pw, ph, mic, sys_audio)
                    .map_err(|e| format!("启动录制失败：{e}"))?;
            // 等待：到时长上限，或停止文件出现（谁先到听谁的）
            let stopped = wait_for_stop(seconds, stop_file.as_deref());
            if let Some(sf) = &stop_file {
                let _ = std::fs::remove_file(sf);
            }
            let stop = recorder::stop(false).map_err(|e| format!("停止录制失败：{e}"))?;
            let src = stop.path.ok_or_else(|| "录制未产生文件".to_string())?;
            // 以实际录制时长为终点，避免补帧边界导致选段超出片长
            let end = if stop.seconds > 0.3 { stop.seconds } else { seconds };
            let exported = recorder::export(&src, 0.0, end, &out, fmt);
            let _ = recorder::discard(&src);
            let path = exported?;
            let region_json = if camera {
                serde_json::Value::Null
            } else {
                json!({"x": px, "y": py, "width": pw, "height": ph})
            };
            Ok(json!({
                "ok": true,
                "command": "record",
                "camera": camera,
                "path": path,
                "format": fmt,
                "seconds": end,
                "region": region_json,
                "mic": info.mic,
                "sys_audio": info.sys_audio,
                "audio_warnings": info.audio_warnings,
                "stopped_by": if stopped { "stop_file" } else { "duration" },
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
        assert!(!wants_cli(&args(&["--no-such"])), "未知首参应交给 GUI/报错路径");
        assert!(!wants_cli(&[]), "无参数 = 正常启动 GUI");
    }

    #[test]
    fn parse_screenshot_ok() {
        let j = parse(&args(&["screenshot", "--out", r"D:\a.png", "--region", "0,0,800,600"]))
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
            } => {
                assert_eq!(out, "demo.gif");
                assert_eq!(seconds, 12.5);
                assert_eq!(region, None);
                assert!(mic && sys_audio);
                assert!(!camera, "缺省为屏幕录制");
                assert!(stop_file.is_none(), "未给 --stop-file 时为空");
            }
            _ => panic!("命令类型错误"),
        }
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
            parse(&args(&["screenshot", "--out", "a.png", "--stop-file", "f"]))
                .is_err(),
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
        // 三个子命令都在
        let names: Vec<&str> = d["commands"]
            .as_array()
            .expect("commands 应为数组")
            .iter()
            .map(|c| c["name"].as_str().expect("name 应为字符串"))
            .collect();
        assert_eq!(names, vec!["screenshot", "record", "help"]);
        // record 的声明里必须能查到 --stop-file 的用法
        let record = &d["commands"][1];
        assert!(
            record["optional"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "--stop-file"),
            "record 声明需包含 --stop-file"
        );
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
        let j =
            parse(&args(&["record", "--camera", "--seconds", "5", "--out", "cam.mp4", "--mic"]))
                .expect("--camera 应解析成功");
        match j {
            Job::Record {
                camera, region, mic, ..
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
            parse(&args(&["screenshot", "--out", "a.png", "--region", "1,2,3"])).is_err(),
            "region 三段应报错"
        );
        assert!(
            parse(&args(&["screenshot", "--out", "a.png", "--region", "0,0,1,1"])).is_err(),
            "region 过小应报错"
        );
        assert!(parse(&args(&["nope"])).is_err(), "未知命令应报错");
    }

    #[test]
    fn parse_region_values() {
        assert_eq!(parse_region(" 10 , 20 , 300 , 400 ").unwrap(), (10, 20, 300, 400));
        assert_eq!(parse_region("-10,-20,300,400").unwrap(), (-10, -20, 300, 400));
    }
}
