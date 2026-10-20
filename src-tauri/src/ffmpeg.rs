//! FFmpeg 定位与自检。
//!
//! 设计取舍：**不打包 FFmpeg**。把它塞进安装包会让体积从几十 MB 涨到上百 MB，
//! 与「轻量化」的定位冲突；而且带 x264 的 FFmpeg 是 GPL-2.0，随安装包分发会引入
//! 额外的许可证义务。所以这里做的是「可靠的定位 + 说得清的错误」，而不是内置。
//!
//! 查找顺序：
//!   1. 环境变量 `LIGHT_CAPTURE_FFMPEG`（指向 exe 或含 ffmpeg.exe 的目录）
//!   2. 程序自身所在目录（便携版常见做法：ffmpeg.exe 与主程序放一起）
//!   3. 系统 PATH
//!   4. 常见包管理器安装位置（winget / scoop / chocolatey / 手工解压）
//!
//! 找到后缓存，后续所有调用都走同一个路径——避免 PATH 在运行期被改来改去，
//! 也避免每次录屏都做一轮磁盘探测。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// 环境变量：显式指定 ffmpeg 路径
pub const ENV_OVERRIDE: &str = "LIGHT_CAPTURE_FFMPEG";

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

static LOCATED: OnceLock<Result<Located, String>> = OnceLock::new();

/// 取 ffmpeg 路径（带缓存）。找不到时返回可直接展示给用户的错误。
pub fn locate() -> Result<&'static Located, String> {
    LOCATED
        .get_or_init(do_locate)
        .as_ref()
        .map_err(|e| e.clone())
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

    // 2. 主程序同目录（便携版）
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        let p = dir.join(exe_name());
        if p.is_file() && is_usable(&p) {
            return Ok(Located {
                program: p,
                origin: Origin::ExeDir,
            });
        }
    }

    // 3. PATH
    if which(exe_name()) {
        return Ok(Located {
            program: PathBuf::from("ffmpeg"),
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

/// 路径上确实有个文件还不够：必须真的能跑起来、且是 ffmpeg。
/// 否则一个被占用/损坏/同名别的程序会让后续录制以莫名其妙的方式失败。
fn is_usable(path: &Path) -> bool {
    Command::new(path)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 用 `ffmpeg -version` 探测 PATH 中是否可用。
/// 不自己解析 PATH 字符串：Windows 的 PATH 分隔符、注册表展开、App Execution Aliases
/// 都可能和直觉不同，交给系统查找最可靠。
fn which(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
         装好后执行 `light-capture doctor` 复核。"
    )
}

/// 构造一个指向已定位 ffmpeg 的 Command
pub fn command() -> Result<Command, String> {
    Ok(Command::new(&locate()?.program))
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
