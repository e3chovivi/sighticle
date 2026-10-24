//! 用户设置：全局快捷键、音频增益、开机自启。
//! 持久化到 `<数据目录>/settings.json`，读写都走这里，避免各处散落。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// 默认唤起键位
pub const DEFAULT_HOTKEY: &str = "ctrl+shift+q";
/// 测试期强杀键位（固定，不允许用户改，避免把自己锁死）
pub const KILL_HOTKEY: &str = "ctrl+alt+shift+q";

/// 麦克风增益的合法范围（1.0 = 原始音量）
pub const MIC_GAIN_RANGE: (f32, f32) = (1.0, 6.0);
/// 系统声音增益的合法范围
pub const SYS_GAIN_RANGE: (f32, f32) = (0.2, 2.0);

/// 字幕识别配置：填了对应服务商的凭证且 enabled 才生效（没填 = 功能不存在）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SubtitleCfg {
    /// 总开关（导出录屏后自动生成 .srt；设置页可在凭证齐全时切换）
    pub enabled: bool,
    /// 服务商："qwen"（阿里 Qwen3-ASR-Flash，默认）| "doubao"（豆包极速版）
    pub provider: String,
    /// 阿里云百炼 DashScope API Key
    pub qwen_api_key: String,
    /// 火山引擎 App Key（豆包，旧称 APP ID）
    pub doubao_app_key: String,
    /// 火山引擎 Access Token（旧版控制台使用；新版控制台可留空）
    pub doubao_access_token: String,
}

impl SubtitleCfg {
    /// 当前服务商的凭证是否齐全
    pub fn creds_ready(&self) -> bool {
        match self.provider.as_str() {
            "doubao" => !self.doubao_app_key.trim().is_empty(),
            _ => !self.qwen_api_key.trim().is_empty(),
        }
    }

    /// 字幕功能实际可用：开关打开且凭证齐全
    pub fn usable(&self) -> bool {
        self.enabled && self.creds_ready()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 唤起键位（规范化形式，如 "ctrl+shift+q"）
    pub hotkey: String,
    /// 麦克风增益：麦克风偏小时可调大；过大可能削波
    pub mic_gain: f32,
    /// 系统声音增益
    pub sys_gain: f32,
    /// 是否开机自启
    pub autostart: bool,
    /// 是否把鼠标指针画进录屏画面（xcap 默认不采集指针）
    pub cursor_overlay: bool,
    /// 是否在指针周围叠加蓝色光晕（指针更醒目，便于 AI 回看）
    pub cursor_glow: bool,
    /// 截图默认保存目录：None = 每次询问保存位置
    pub save_dir: Option<String>,
    /// 字幕识别配置（没填凭证 = 功能不存在）
    pub subtitle: SubtitleCfg,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: DEFAULT_HOTKEY.to_string(),
            mic_gain: 1.0,
            sys_gain: 1.0,
            autostart: false,
            cursor_overlay: true,
            cursor_glow: true,
            save_dir: None,
            subtitle: SubtitleCfg::default(),
        }
    }
}

impl Settings {
    /// 把数值收敛到合法范围，避免手改配置或历史脏数据导致异常
    fn sanitize(&mut self) {
        self.mic_gain = self.mic_gain.clamp(MIC_GAIN_RANGE.0, MIC_GAIN_RANGE.1);
        self.sys_gain = self.sys_gain.clamp(SYS_GAIN_RANGE.0, SYS_GAIN_RANGE.1);
        if self.hotkey.trim().is_empty() {
            self.hotkey = DEFAULT_HOTKEY.to_string();
        }
        // 空白目录等价于「每次询问」
        self.save_dir = self
            .save_dir
            .take()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());
        // 字幕：服务商规范化 + 凭证去空白；凭证不齐时开关自动关（没填 = 功能不存在）
        let sub = &mut self.subtitle;
        sub.provider = if sub.provider.trim().to_lowercase() == "doubao" {
            "doubao".to_string()
        } else {
            "qwen".to_string()
        };
        sub.qwen_api_key = sub.qwen_api_key.trim().to_string();
        sub.doubao_app_key = sub.doubao_app_key.trim().to_string();
        sub.doubao_access_token = sub.doubao_access_token.trim().to_string();
        if !sub.creds_ready() {
            sub.enabled = false;
        }
    }
}

static STATE: OnceLock<Mutex<Settings>> = OnceLock::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();

fn state() -> &'static Mutex<Settings> {
    STATE.get_or_init(|| Mutex::new(Settings::default()))
}

fn path() -> Option<&'static PathBuf> {
    PATH.get()
}

/// 旧版应用标识对应的设置文件。标识符从模板值改为正式值后，
/// 老用户的快捷键/增益/凭证不该凭空丢失，首次启动时迁移一次。
fn legacy_settings_paths() -> Vec<PathBuf> {
    match std::env::var("LOCALAPPDATA") {
        Ok(v) => crate::LEGACY_IDENTIFIERS
            .iter()
            .map(|id| PathBuf::from(&v).join(id).join("settings.json"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 只有当 `dir` 确实是正式的应用数据目录时才做迁移。
/// 测试与自定义目录（如临时目录）不应读到用户本机的真实配置。
fn is_app_data_dir(dir: &Path) -> bool {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|v| PathBuf::from(v).join(crate::APP_IDENTIFIER) == dir)
        .unwrap_or(false)
}

/// 从磁盘加载设置（不存在则用默认值）。应在启动早期调用一次。
pub fn init(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let p = dir.join("settings.json");
    if !p.exists() && is_app_data_dir(dir) {
        for legacy in legacy_settings_paths() {
            if legacy.exists() {
                let _ = std::fs::copy(&legacy, &p);
                break;
            }
        }
    }
    let mut loaded = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .unwrap_or_default();
    loaded.sanitize();
    let _ = PATH.set(p);
    if let Ok(mut g) = state().lock() {
        *g = loaded;
    }
}

/// 读取当前设置副本
pub fn get() -> Settings {
    state().lock().map(|g| g.clone()).unwrap_or_default()
}

/// 修改设置并落盘。f 返回 Err 时不写入。
pub fn update<F>(f: F) -> Result<Settings, String>
where
    F: FnOnce(&mut Settings) -> Result<(), String>,
{
    let mut g = state().lock().map_err(|_| "设置状态不可用".to_string())?;
    let mut next = g.clone();
    f(&mut next)?;
    next.sanitize();
    save(&next)?;
    *g = next.clone();
    Ok(next)
}

fn save(s: &Settings) -> Result<(), String> {
    let p = path().ok_or_else(|| "设置路径未初始化".to_string())?;
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(p, text).map_err(|e| format!("写入设置失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let s = Settings::default();
        assert_eq!(s.hotkey, DEFAULT_HOTKEY);
        assert_eq!(s.mic_gain, 1.0);
        assert!(!s.autostart);
    }

    #[test]
    fn sanitize_clamps_out_of_range_gains() {
        let mut s = Settings {
            mic_gain: 99.0,
            sys_gain: 0.0,
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.mic_gain, MIC_GAIN_RANGE.1, "超上限应被夹到上限");
        assert_eq!(s.sys_gain, SYS_GAIN_RANGE.0, "低于下限应被夹到下限");
    }

    #[test]
    fn sanitize_restores_empty_hotkey() {
        let mut s = Settings {
            hotkey: "   ".into(),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.hotkey, DEFAULT_HOTKEY);
    }

    #[test]
    fn sanitize_clears_blank_save_dir() {
        let mut s = Settings {
            save_dir: Some("   ".into()),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.save_dir, None, "空白目录应等价于「每次询问」");

        let mut s = Settings {
            save_dir: Some(" D:\\shots ".into()),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.save_dir.as_deref(), Some("D:\\shots"), "应去掉首尾空白");
    }

    #[test]
    fn sanitize_subtitle_cfg() {
        // 凭证不齐 → 开关强制关闭（没填 = 功能不存在）
        let mut s = Settings {
            subtitle: SubtitleCfg {
                enabled: true,
                provider: "qwen".into(),
                qwen_api_key: "  ".into(),
                ..SubtitleCfg::default()
            },
            ..Settings::default()
        };
        s.sanitize();
        assert!(!s.subtitle.enabled, "空 key 时开关应被强制关闭");
        assert!(!s.subtitle.usable());

        // 凭证齐全 → 开关保留
        let mut s = Settings {
            subtitle: SubtitleCfg {
                enabled: true,
                provider: "qwen".into(),
                qwen_api_key: " sk-test ".into(),
                ..SubtitleCfg::default()
            },
            ..Settings::default()
        };
        s.sanitize();
        assert!(s.subtitle.enabled && s.subtitle.usable());
        assert_eq!(s.subtitle.qwen_api_key, "sk-test", "凭证应去空白");

        // provider 规范化
        let mut s = Settings {
            subtitle: SubtitleCfg {
                provider: "DOUBAO ".into(),
                doubao_app_key: "123".into(),
                ..SubtitleCfg::default()
            },
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.subtitle.provider, "doubao");
        assert!(s.subtitle.creds_ready(), "豆包只需 App Key");

        let mut s = Settings {
            subtitle: SubtitleCfg {
                provider: "unknown".into(),
                ..SubtitleCfg::default()
            },
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.subtitle.provider, "qwen", "未知服务商回退默认");
    }

    #[test]
    fn update_rejects_and_keeps_old_value_on_error() {
        init(std::env::temp_dir().as_path());
        let before = get();
        let res = update(|s| {
            s.mic_gain = 3.0;
            Err("故意失败".into())
        });
        assert!(res.is_err());
        assert_eq!(get().mic_gain, before.mic_gain, "校验失败时不得写入");
    }

    #[test]
    fn update_persists_and_clamps() {
        init(std::env::temp_dir().as_path());
        let s = update(|s| {
            s.mic_gain = 2.5;
            Ok(())
        })
        .expect("应写入成功");
        assert_eq!(s.mic_gain, 2.5);
        assert_eq!(get().mic_gain, 2.5);
    }
}
