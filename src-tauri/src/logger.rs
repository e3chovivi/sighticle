//! 极简文件日志：把关键流程与**原始错误详情**落到 `<数据目录>/logs/light-capture.log`
//!
//! 为什么需要它：屏幕采集（WGC）与音频采集（WASAPI）的失败往往带 HRESULT 码，
//! 而 GUI 里只能弹一句笼统的提示。把每次尝试的原始错误、耗时、上下文写进文件后，
//! 用户复现一次即可事后精确定位，不必反复猜测。
//!
//! 设计取舍：不引入 log/tracing 生态（省依赖、省体积），手写一个带时间戳与线程名的
//! 追加式日志，并按大小做一次简单轮转。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// 单个日志文件的大小上限，超出则轮转为 .1
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

static LOG_FILE: OnceLock<Mutex<fs::File>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
/// 是否把日志同步打到 stdout。GUI 模式开着便于观察；CLI 模式必须关掉——
/// 契约要求 stdout 只有结果那一行 JSON，日志只进文件。
static STDOUT_ECHO: AtomicBool = AtomicBool::new(true);

/// 关闭/开启 stdout 日志回显（CLI 模式启动时调用，保证 stdout 单行 JSON）
pub fn set_stdout_echo(enabled: bool) {
    STDOUT_ECHO.store(enabled, Ordering::Relaxed);
}

/// 初始化日志（应在应用启动最早阶段调用）。dir 通常为 Tauri 的 app_local_data_dir。
pub fn init(dir: &Path) {
    let logs_dir = dir.join("logs");
    if fs::create_dir_all(&logs_dir).is_err() {
        return;
    }
    let path = logs_dir.join("light-capture.log");
    // 简单的按大小轮转：超过上限就改名为 .1（覆盖旧的一份）
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            let _ = fs::rename(&path, logs_dir.join("light-capture.log.1"));
        }
    }
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => {
            let _ = LOG_FILE.set(Mutex::new(f));
            let _ = LOG_PATH.set(path.clone());
            log(&format!(
                "===== light-capture v{} 启动，日志文件：{} =====",
                env!("CARGO_PKG_VERSION"),
                path.display()
            ));
        }
        Err(_) => {
            // 日志初始化失败不应影响主流程
        }
    }
    install_panic_hook();
}

/// 当前日志文件路径（用于在界面上告知用户 / 排查时定位）
pub fn path() -> Option<PathBuf> {
    LOG_PATH.get().cloned()
}

/// 目录选择策略：优先用 `preferred`（能创建或已存在就采用），否则回退到 `fallback`。
///
/// 用途：用户可能通过 `LIGHT_CAPTURE_LOG_DIR` 把日志改到别的盘（别占系统盘），
/// 但目标盘不存在或不可写时不能因此丢掉日志能力，所以拿不到首选目录时必须静默回退。
pub fn prefer_dir(preferred: &Path, fallback: &Path) -> PathBuf {
    if fs::create_dir_all(preferred).is_ok() {
        preferred.to_path_buf()
    } else {
        fallback.to_path_buf()
    }
}

/// 写一行日志（带本地时间与线程名）。GUI 模式同步打到 stdout 便于开发观察；
/// CLI 模式（`set_stdout_echo(false)`）只写文件，保证 stdout 单行 JSON。
pub fn log(msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("unnamed");
    let line = format!("[{ts}] [{name}] {msg}");
    if STDOUT_ECHO.load(Ordering::Relaxed) {
        println!("{line}");
    }
    if let Some(f) = LOG_FILE.get() {
        if let Ok(mut g) = f.lock() {
            let _ = writeln!(g, "{line}");
            let _ = g.flush();
        }
    }
}

/// panic 也要留痕：GUI 模式下崩溃信息否则会完全丢失
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".into());
        log(&format!("!! panic at {location}: {info}"));
        default_hook(info);
    }));
}

/// 便捷宏：`lclog!("格式化 {}", x)`
#[macro_export]
macro_rules! lclog {
    ($($arg:tt)*) => {
        $crate::logger::log(&format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lc-logger-{}-{name}", std::process::id()))
    }

    #[test]
    fn prefer_dir_uses_writable_preferred() {
        let preferred = temp_path("preferred");
        let fallback = temp_path("fallback");
        let got = prefer_dir(&preferred, &fallback);
        assert_eq!(got, preferred);
        let _ = fs::remove_dir_all(&got);
    }

    /// 首选目录不可用（目标盘不存在 / 无权限）时必须回退，不能丢掉日志能力
    #[test]
    fn prefer_dir_falls_back_when_preferred_unavailable() {
        // 拿一个"文件"当父目录，制造必然失败的 create_dir_all
        let blocker = temp_path("blocker");
        let _ = fs::remove_dir_all(&blocker);
        let _ = fs::remove_file(&blocker);
        fs::write(&blocker, b"x").expect("准备 blocker 文件失败");
        let preferred = blocker.join("logs");
        let fallback = temp_path("fallback");
        let got = prefer_dir(&preferred, &fallback);
        assert_eq!(got, fallback);
        let _ = fs::remove_file(&blocker);
    }
}
