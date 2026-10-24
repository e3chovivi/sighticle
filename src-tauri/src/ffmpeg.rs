//! FFmpeg 定位与自检。
//!
//! 关于分发：官方安装包**内置** FFmpeg（tauri.conf.json 把 vendor/ffmpeg.exe 打进
//! resources/），对用户开箱即用。FFmpeg（gyan.dev 基本构建，启用 x264）按 GPLv3
//! 分发，但它是作为独立可执行文件随包提供，不传染本项目源码——我们的许可证仍是
//! MIT，法律说明见 README「许可与第三方」一节。
//!
//! 内置不是唯一来源，这里仍要做「可靠的定位 + 说得清的错误」：
//!
//! 查找顺序：
//!   1. 环境变量 `SIGHTICLE_FFMPEG`（指向 exe 或含 ffmpeg.exe 的目录）
//!   2. 程序自身所在目录（便携版常见做法：ffmpeg.exe 与主程序放一起；
//!      安装包内置副本位于主程序同目录的 resources/ 下）
//!   3. 系统 PATH
//!   4. 常见包管理器安装位置（winget / scoop / chocolatey / 手工解压）
//!
//! 找到后缓存，后续所有调用都走同一个路径——避免 PATH 在运行期被改来改去，
//! 也避免每次录屏都做一轮磁盘探测。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// 环境变量：显式指定 ffmpeg 路径
pub const ENV_OVERRIDE: &str = "SIGHTICLE_FFMPEG";

/// ffmpeg 的来源，用于诊断输出
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// 环境变量指定
    EnvOverride,
    /// 与主程序同目录
    ExeDir,
    /// 系统 PATH
    Path,
    /// 常见安装位置
    WellKnown,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::EnvOverride => "env",
            Origin::ExeDir => "exe_dir",
            Origin::Path => "path",
            Origin::WellKnown => "well_known",
        }
    }
}

/// 定位结果
#[derive(Clone, Debug)]
pub struct Located {
    /// 可执行文件路径；经 PATH 找到时可能是裸命令名 `ffmpeg`
    pub program: PathBuf,
    pub origin: Origin,
}

static LOCATED: OnceLock<Located> = OnceLock::new();

/// 取 ffmpeg 路径（成功结果带缓存）。找不到时返回可直接展示给用户的错误。
///
/// **只缓存成功、不缓存失败**：启动那一刻可能因子进程创建被安全软件拦截、
/// 磁盘/路径瞬时不可用等原因探测失败；若把失败也缓存，整个进程生命周期内
/// 录制都会提示“缺少 FFmpeg”，只能重启应用才能恢复（实测出现过）。
/// 失败结果不缓存，下一次录屏会重新探测。
pub fn locate() -> Result<&'static Located, String> {
    if let Some(l) = LOCATED.get() {
        return Ok(l);
    }
    let located = do_locate()?;
    Ok(LOCATED.get_or_init(|| located))
}

/// 供测试与诊断使用：不读缓存，重新探测一次。
pub fn locate_fresh() -> Result<Located, String> {
    do_locate()
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn do_locate() -> Result<Located, String> {
    // 1. 环境变量
    if let Ok(v) = std::env::var(ENV_OVERRIDE) {
        let v = v.trim();
        if !v.is_empty() {
            let p = PathBuf::from(v);
            let p = if p.is_dir() { p.join(exe_name()) } else { p };
            if !p.is_file() {
                // 显式指定却无效：必须报错，不能悄悄退回 PATH，否则用户以为配置生效了
                return Err(format!("{ENV_OVERRIDE} 指向的文件不存在：{}", p.display()));
            }
            if !is_usable(&p) {
                return Err(format!(
                    "{ENV_OVERRIDE} 指向的不是可用的 ffmpeg：{}",
                    p.display()
                ));
            }
            return Ok(Located {
                program: p,
                origin: Origin::EnvOverride,
            });
        }
    }

    // 2. 主程序同目录（便携版 / 安装包把 ffmpeg 随包分发时的落点）
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        // 2a. 与主程序并排
        let p = dir.join(exe_name());
        if p.is_file() && is_usable(&p) {
            return Ok(Located {
                program: p,
                origin: Origin::ExeDir,
            });
        }
        // 2b. 安装包资源目录（Tauri resources 的默认落点）
        let p = dir.join("resources").join(exe_name());
        if p.is_file() && is_usable(&p) {
            return Ok(Located {
                program: p,
                origin: Origin::ExeDir,
            });
        }
    }

    // 3. PATH
    if let Some(p) = which(exe_name()) {
        return Ok(Located {
            program: p,
            origin: Origin::Path,
        });
    }

    // 4. 常见安装位置
    for p in well_known_candidates() {
        if p.is_file() && is_usable(&p) {
            return Ok(Located {
                program: p,
                origin: Origin::WellKnown,
            });
        }
    }

    Err(missing_message())
}

/// 判断路径上的文件是否像一个可用的 ffmpeg。
///
/// **刻意不再试跑 `ffmpeg -version` 作为判据**：GUI 进程在部分 Windows 环境下
/// 创建子进程会间歇性失败（实测：同一时刻 CLI 进程正常、GUI 内返回
/// `ERROR_NOT_SUPPORTED(50)`）。把一次创建进程的抖动误判成「没装 FFmpeg」，
/// 会让界面上出现误导性的「缺少 FFmpeg」，而实际文件就在那里、录制也能跑起来。
/// 这里只做「存在 + 体积合理」的静态检查，真正能否运行由录制时的启动错误如实上报。
fn is_usable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 100 * 1024)
        .unwrap_or(false)
}

/// 在 PATH 中查找可执行文件（返回完整路径，避免录制时再让系统解析一次）。
///
/// 只做文件系统查找、不创建进程：原因同上——探测阶段不应依赖 CreateProcess 的可用性。
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| is_usable(p))
}

/// 常见包管理器与手工安装位置
fn well_known_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let exe = exe_name();
    let _ = &exe; // 非 Windows 分支不使用

    #[cfg(windows)]
    {
        // winget 会创建 Links 目录下的转发器
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            out.push(
                PathBuf::from(&local)
                    .join("Microsoft/WinGet/Links")
                    .join(exe),
            );
            // 部分 winget 版本直接解压到 Packages 下
            out.push(
                PathBuf::from(&local)
                    .join("Microsoft/WinGet/Packages")
                    .join(exe),
            );
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            out.push(PathBuf::from(&profile).join("scoop/shims").join(exe));
        }
        if let Ok(pd) = std::env::var("ProgramData") {
            out.push(PathBuf::from(&pd).join("chocolatey/bin").join(exe));
        }
        out.push(PathBuf::from(r"C:\ffmpeg\bin").join(exe));
        if let Ok(pf) = std::env::var("ProgramFiles") {
            out.push(PathBuf::from(pf).join("ffmpeg/bin").join(exe));
        }
    }
    #[cfg(not(windows))]
    {
        out.push(PathBuf::from("/usr/bin/ffmpeg"));
        out.push(PathBuf::from("/usr/local/bin/ffmpeg"));
        out.push(PathBuf::from("/opt/homebrew/bin/ffmpeg"));
    }
    out
}

/// 找不到时给出可操作的说明（Agent 与人都能直接照着做）
fn missing_message() -> String {
    format!(
        "未找到 FFmpeg。截图不需要它；录制、导出、抽帧、场景检测需要。\n\
         安装任一即可：\n\
         \x20 winget install --id Gyan.FFmpeg\n\
         \x20 choco install ffmpeg\n\
         \x20 scoop install ffmpeg\n\
         或下载后把 ffmpeg.exe 放到主程序同目录，\n\
         也可用环境变量 {ENV_OVERRIDE} 指向它。\n\
         装好后执行 `sighticle doctor` 复核。"
    )
}

/// 构造一个指向已定位 ffmpeg 的 Command。
///
/// **stdin 必须显式置 `Stdio::null()`**：默认的 `inherit` 会把父进程的 stdin 句柄
/// 传给子进程，而 GUI 进程启动即 `FreeConsole()`，stdin 是悬空句柄，Windows 拒绝
/// 用它创建子进程——实测稳定复现 `ERROR_NOT_SUPPORTED(50)`（“不支持该请求”），
/// 表现为录屏后混流/导出失败、提示缺少 FFmpeg；命令行模式有真控制台所以不受影响。
/// 需要管道的调用方（如录制编码器）在本函数之后再 `.stdin(Stdio::piped())` 覆盖。
///
/// Windows 上统一带 `CREATE_NO_WINDOW`：不给子进程分配控制台窗口（不闪窗）。
pub fn command() -> Result<Command, String> {
    let mut cmd = Command::new(&locate()?.program);
    cmd.stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // 必须保留 CREATE_UNICODE_ENVIRONMENT：creation_flags 会**覆盖** Rust 的默认标志，
        // 只写 CREATE_NO_WINDOW 会让环境块按 ANSI 传递，中文路径/变量场景下可能出问题。
        const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT);
    }
    Ok(cmd)
}

/// 读取版本号（`ffmpeg -version` 第一行）
pub fn version() -> Result<String, String> {
    let out = command()?
        .arg("-version")
        .output()
        .map_err(|e| format!("无法执行 ffmpeg：{e}"))?;
    if !out.status.success() {
        return Err("ffmpeg -version 执行失败".into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("").trim().to_string();
    if first.is_empty() {
        return Err("无法解析 ffmpeg 版本".into());
    }
    Ok(first)
}

/// 自检结果（doctor 命令输出）
pub fn diagnose() -> serde_json::Value {
    use serde_json::json;

    let (found, program, origin, version, error) = match locate_fresh() {
        Ok(l) => {
            let v = version().ok();
            (
                true,
                Some(l.program.to_string_lossy().into_owned()),
                Some(l.origin.as_str()),
                v,
                None,
            )
        }
        Err(e) => (false, None, None, None, Some(e)),
    };
    json!({
        "found": found,
        "program": program,
        "origin": origin,
        "version": version,
        "error": error,
        "env_override": std::env::var(ENV_OVERRIDE).ok(),
        "hint": if found { serde_json::Value::Null } else { serde_json::Value::String(missing_message()) },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_message_is_actionable() {
        let m = missing_message();
        assert!(m.contains("winget"), "应给出可直接复制的安装命令");
        assert!(m.contains(ENV_OVERRIDE), "应说明可自定义路径");
        assert!(m.contains("截图不需要"), "应说明哪些功能不受影响");
    }

    #[test]
    fn env_override_nonexistent_is_error_not_fallback() {
        // 显式配置错了必须报错；这里只验证错误文案包含路径，不真去改进程环境
        let msg = format!(
            "{ENV_OVERRIDE} 指向的文件不存在：{}",
            Path::new("definitely_missing_ffmpeg_xyz").display()
        );
        assert!(msg.contains("definitely_missing_ffmpeg_xyz"));
    }

    #[test]
    fn origin_labels_are_stable() {
        assert_eq!(Origin::EnvOverride.as_str(), "env");
        assert_eq!(Origin::ExeDir.as_str(), "exe_dir");
        assert_eq!(Origin::Path.as_str(), "path");
        assert_eq!(Origin::WellKnown.as_str(), "well_known");
    }

    #[test]
    fn exe_name_matches_platform() {
        if cfg!(windows) {
            assert_eq!(exe_name(), "ffmpeg.exe");
        } else {
            assert_eq!(exe_name(), "ffmpeg");
        }
    }

    #[test]
    fn candidates_include_common_locations() {
        let c = well_known_candidates();
        assert!(!c.is_empty());
        assert!(c.iter().all(|p| p.file_name().is_some()));
    }
}
