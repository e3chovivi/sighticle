//! 字幕识别：导出的录屏 → 提取音轨 → 服务商 ASR（句级时间戳）→ 生成 SRT。
//!
//! 支持两家服务商（设置页可切换，凭证存在 settings.json）：
//! - **qwen（默认）**：阿里云百炼 `qwen3-asr-flash-filetrans`，异步任务。
//!   流程：上传 mp3 到 DashScope 临时存储（OSS 表单，1GB 内）→ 提交任务 → 轮询。
//!   结果 `sentences[]` 带句级 begin/end 时间戳（毫秒）。
//! - **doubao**：豆包录音文件极速版 `recognize/flash`，base64 直传，通常一次请求
//!   即返回；状态码 20000001/20000002（创建/运行中）时用同一 Request-Id 轮询。
//!   结果 `result.utterances[]` 带句级 start/end 时间戳（毫秒）。
//!
//! 时间戳对齐说明：音轨从**导出后的文件**提取（从 0 开始），ASR 时间戳天然对齐
//! 该文件，时间轴裁剪的偏移无需另行换算。
//!
//! 未配置凭证时 `generate_subtitle` 直接返回空串（功能不存在，前端静默跳过）。

use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::settings::SubtitleCfg;

/// 一句字幕（毫秒时间戳，相对音频起点）
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

// ================= 对外入口 =================

/// 字幕识别结果：SRT 文件路径 + 是否已内嵌进视频。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubtitleOutcome {
    /// 同名 .srt 文件路径（未启用时为空串）
    pub srt: String,
    /// 字幕是否已烧录进画面（任何播放器点开即见）
    pub embedded: bool,
}

/// 为导出的录屏生成字幕：识别 → 写同名 .srt → **烧录进画面**（硬字幕）。
///
/// 为什么烧录而不是字幕轨：软字幕轨（mov_text）很多播放器不自动显示，
/// 用户点开视频看不到字幕。烧录后任何播放器点开即见，同时保留 .srt 给 AI 读取。
///
/// 返回：
/// - `Ok(out)`：`srt` 为空串 = 字幕功能未启用/未配置（静默跳过，不算错误）；
/// - `Err(msg)`：识别失败（已写日志）。内嵌失败不算错误，只置 `embedded=false`。
pub fn generate_subtitle(video: &str) -> Result<SubtitleOutcome, String> {
    let cfg = crate::settings::get().subtitle;
    if !cfg.usable() {
        return Ok(SubtitleOutcome { srt: String::new(), embedded: false });
    }
    let video_path = Path::new(video);
    if !video_path.is_file() {
        return Err(format!("视频文件不存在：{video}"));
    }
    crate::lclog!(
        "字幕识别开始：provider={} 文件={video}",
        cfg.provider
    );

    let audio = extract_audio(video_path)?;
    let cues = match cfg.provider.as_str() {
        "doubao" => transcribe_doubao(&audio, &cfg)?,
        _ => transcribe_qwen(&audio, &cfg)?,
    };
    if cues.is_empty() {
        return Err("未识别到语音内容".into());
    }

    let srt_path = video_path.with_extension("srt");
    std::fs::write(&srt_path, build_srt(&cues)).map_err(|e| format!("写入字幕失败：{e}"))?;
    let embedded = burn_subtitle_track(video_path, &srt_path);
    crate::lclog!(
        "字幕生成完成：{}（{} 句，内嵌={embedded}）",
        srt_path.display(),
        cues.len()
    );
    Ok(SubtitleOutcome {
        srt: srt_path.to_string_lossy().into_owned(),
        embedded,
    })
}

/// 把字幕**烧录进画面**（硬字幕）：任何播放器点开即见，不依赖播放器
/// 的字幕轨支持（软字幕 mov_text 常被播放器忽略，用户看不到）。
///
/// 做法：SRT 先复制到纯 ASCII 临时路径（ffmpeg 滤镜对含盘符冒号/中文的
/// 路径兼容性差），烧录输出临时文件后安全替换原片（失败自动回滚，绝不丢原片）。
fn burn_subtitle_track(video: &Path, srt: &Path) -> bool {
    let tmp_srt = std::env::temp_dir().join(format!("lc_sub_{}.srt", crate::stamp()));
    if std::fs::copy(srt, &tmp_srt).is_err() {
        crate::lclog!("字幕烧录失败：无法复制字幕到临时目录");
        return false;
    }
    let tmp = video.with_extension("burn_tmp.mp4");
    let args = build_burn_args(
        &video.to_string_lossy(),
        &tmp_srt.to_string_lossy(),
        &tmp.to_string_lossy(),
    );
    let ok = crate::recorder::run_ffmpeg(&args, Duration::from_secs(600)).is_ok();
    let _ = std::fs::remove_file(&tmp_srt);
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        crate::lclog!("字幕烧录失败（.srt 已保留）");
        return false;
    }
    // Windows 的 rename 不能覆盖已存在文件：原片挪到 .bak，新片就位后删 .bak；
    // 新片就位失败则把原片挪回来，保证任何一步失败原视频都还在。
    let bak = video.with_extension("orig_tmp.mp4");
    if std::fs::rename(video, &bak).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    match std::fs::rename(&tmp, video) {
        Ok(()) => {
            let _ = std::fs::remove_file(&bak);
            true
        }
        Err(_) => {
            let _ = std::fs::rename(&bak, video);
            let _ = std::fs::remove_file(&tmp);
            false
        }
    }
}

/// 拼装"字幕烧录进画面"的参数：`subtitles` 滤镜 + libx264 重编码画面，
/// 音轨直接 copy 不重编码。
///
/// srt 路径进滤镜前必须转义：`\`→`/`、`:`→`\:`，再用单引号包住
/// （ffmpeg 滤镜语法里冒号是参数分隔符，不转义会把盘符截断）。
pub fn build_burn_args(video: &str, srt: &str, out: &str) -> Vec<String> {
    let escaped = srt.replace('\\', "/").replace(':', "\\:");
    let vf = format!(
        "subtitles='{escaped}':force_style='FontName=Microsoft YaHei,FontSize=16,Outline=1,MarginV=18'"
    );
    [
        "-y".to_string(),
        "-i".to_string(),
        video.to_string(),
        "-vf".to_string(),
        vf,
        "-c:v".to_string(),
        "libx264".to_string(),
        "-preset".to_string(),
        "veryfast".to_string(),
        "-crf".to_string(),
        "23".to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
        "-c:a".to_string(),
        "copy".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        out.to_string(),
    ]
    .to_vec()
}

// ================= 音轨提取 =================

/// 从视频提取音频：16kHz 单声道 MP3 64kbps。
/// mp3 体积小（10 分钟约 4.8MB，base64 后 ~6.5MB，各家限制内），两家服务商都支持。
fn extract_audio(video: &Path) -> Result<Vec<u8>, String> {
    let mp3 = video.with_extension("asr_tmp.mp3");
    let args = vec![
        "-y".to_string(),
        "-i".to_string(),
        video.to_string_lossy().into_owned(),
        "-vn".to_string(),
        "-ac".to_string(),
        "1".to_string(),
        "-ar".to_string(),
        "16000".to_string(),
        "-codec:a".to_string(),
        "libmp3lame".to_string(),
        "-b:a".to_string(),
        "64k".to_string(),
        mp3.to_string_lossy().into_owned(),
    ];
    crate::recorder::run_ffmpeg(&args, Duration::from_secs(120))
        .map_err(|e| format!("提取音轨失败：{e}"))?;
    let bytes = std::fs::read(&mp3).map_err(|e| format!("读取音轨失败：{e}"))?;
    let _ = std::fs::remove_file(&mp3);
    if bytes.is_empty() {
        return Err("视频没有可用的音轨".into());
    }
    Ok(bytes)
}

// ================= 豆包（极速版，同步 + 轮询兜底） =================

const DOUBAO_URL: &str = "https://openspeech.bytedance.com/api/v3/auc/bigmodel/recognize/flash";

fn transcribe_doubao(audio: &[u8], cfg: &SubtitleCfg) -> Result<Vec<Cue>, String> {
    let task_id = request_id();
    let audio_b64 = base64::engine::general_purpose::STANDARD.encode(audio);
    let body = json!({
        "user": { "uid": cfg.doubao_app_key },
        "audio": { "data": audio_b64, "format": "mp3", "rate": 16000 },
        "request": { "model_name": "bigmodel" },
    });
    let agent = http_agent();

    // 20000000=成功 20000001=已创建 20000002=运行中（后两者用同一 Request-Id 重发即为查询）
    for attempt in 0..30 {
        let mut req = agent
            .post(DOUBAO_URL)
            .set("X-Api-App-Key", &cfg.doubao_app_key)
            .set("X-Api-Resource-Id", "volc.bigasr.auc_turbo")
            .set("X-Api-Request-Id", &task_id)
            .set("X-Api-Sequence", "-1");
        if !cfg.doubao_access_token.is_empty() {
            req = req.set("X-Api-Access-Key", &cfg.doubao_access_token);
        }
        let resp = match req.send_json(body.clone()) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                // 4xx/5xx：尽量取回服务端信息
                let detail = r
                    .into_string()
                    .unwrap_or_else(|_| "(无响应体)".into());
                return Err(format!("豆包识别失败：HTTP {code} {detail}"));
            }
            Err(e) => return Err(format!("豆包请求失败：{e}")),
        };
        let status = resp.header("X-Api-Status-Code").unwrap_or("").to_string();
        match status.as_str() {
            "20000000" => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| format!("豆包响应解析失败：{e}"))?;
                let cues = parse_doubao(&v);
                crate::lclog!("豆包识别成功：{} 句", cues.len());
                return Ok(cues);
            }
            "20000001" | "20000002" => {
                crate::lclog!("豆包任务 {}（第 {} 次查询）", status, attempt + 1);
                std::thread::sleep(Duration::from_millis(1000));
            }
            other => {
                let msg = resp.header("X-Api-Message").unwrap_or("");
                return Err(format!("豆包识别失败：{other} {msg}"));
            }
        }
    }
    Err("豆包识别超时（60 秒）".into())
}

/// 解析豆包成功响应：`result.utterances[]` → cues
fn parse_doubao(v: &Value) -> Vec<Cue> {
    v.pointer("/result/utterances")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|u| {
                    let text = u.get("text")?.as_str()?.trim();
                    if text.is_empty() {
                        return None;
                    }
                    Some(Cue {
                        start_ms: u.get("start_time")?.as_i64()?,
                        end_ms: u.get("end_time")?.as_i64()?,
                        text: text.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ================= 阿里 Qwen（Filetrans：上传 → 提交 → 轮询） =================

const QWEN_MODEL: &str = "qwen3-asr-flash-filetrans";
const QWEN_UPLOADS: &str = "https://dashscope.aliyuncs.com/api/v1/uploads";
const QWEN_SUBMIT: &str = "https://dashscope.aliyuncs.com/api/v1/services/audio/asr/transcription";
const QWEN_TASKS: &str = "https://dashscope.aliyuncs.com/api/v1/tasks/";

fn transcribe_qwen(audio: &[u8], cfg: &SubtitleCfg) -> Result<Vec<Cue>, String> {
    let agent = http_agent();
    let key = cfg.qwen_api_key.trim();

    // 1) 上传凭证 → 2) OSS 表单直传 → oss:// 临时 URL（48 小时有效）
    let file_url = upload_audio(&agent, key, audio)?;

    // 3) 提交转写任务
    let submit_body = json!({
        "model": QWEN_MODEL,
        "input": { "file_url": file_url },
        "parameters": { "channel_id": [0], "enable_itn": false, "enable_words": true },
    });
    let resp = agent
        .post(QWEN_SUBMIT)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .set("X-DashScope-Async", "enable")
        .set("X-DashScope-OssResourceResolve", "enable")
        .send_json(submit_body)
        .map_err(|e| map_qwen_err(e, "提交任务"))?;
    let v: Value = resp
        .into_json()
        .map_err(|e| format!("提交响应解析失败：{e}"))?;
    let task_id = v
        .pointer("/output/task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("提交任务未返回 task_id：{}", truncate_for_log(&v))
        })?
        .to_string();
    crate::lclog!("Qwen 任务已提交：{task_id}");

    // 4) 轮询（2s 间隔，上限 5 分钟）
    for attempt in 0..150 {
        std::thread::sleep(Duration::from_secs(2));
        let r = agent
            .get(&format!("{QWEN_TASKS}{task_id}"))
            .set("Authorization", &format!("Bearer {key}"))
            .set("Content-Type", "application/json")
            .call();
        let v: Value = match r {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| format!("查询响应解析失败：{e}"))?,
            Err(ureq::Error::Status(code, resp)) => {
                let detail = resp.into_string().unwrap_or_default();
                return Err(format!("查询任务失败：HTTP {code} {detail}"));
            }
            Err(e) => return Err(format!("查询任务失败：{e}")),
        };
        let status = v
            .pointer("/output/task_status")
            .and_then(Value::as_str)
            .unwrap_or("");
        match status {
            "SUCCEEDED" => {
                // 识别结果两种形态：①查询响应内联 sentences；②只给 transcription_url
                // 外链（OSS 签名 URL，免鉴权），需再拉一次才有句子与时间戳。
                let mut cues = parse_qwen_sentences(&v);
                if cues.is_empty() {
                    match find_str_field(&v, "transcription_url") {
                        Some(url) => {
                            crate::lclog!("Qwen 识别结果为外链，拉取识别详情");
                            let detail: Value = agent
                                .get(&url)
                                .call()
                                .map_err(|e| format!("拉取识别结果失败：{e}"))?
                                .into_json()
                                .map_err(|e| format!("识别结果解析失败：{e}"))?;
                            cues = parse_qwen_sentences(&detail);
                        }
                        None => {
                            crate::lclog!(
                                "!! Qwen 结果缺少时间戳，原始响应：{}",
                                truncate_for_log(&v)
                            );
                        }
                    }
                }
                if cues.is_empty() {
                    return Err("识别结果缺少时间戳（原始响应见日志）".into());
                }
                crate::lclog!("Qwen 识别成功：{} 条字幕", cues.len());
                return Ok(cues);
            }
            "FAILED" | "UNKNOWN" => {
                let msg = v
                    .pointer("/output/message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                return Err(format!("Qwen 任务失败：{status} {msg}"));
            }
            _ => {
                if attempt % 15 == 0 {
                    crate::lclog!("Qwen 任务状态：{status}（第 {} 次查询）", attempt + 1);
                }
            }
        }
    }
    Err("Qwen 识别超时（5 分钟）".into())
}

/// DashScope 临时存储两步上传：getPolicy → OSS multipart 表单直传 → oss:// URL
fn upload_audio(agent: &ureq::Agent, key: &str, audio: &[u8]) -> Result<String, String> {
    let policy = agent
        .get(QWEN_UPLOADS)
        .query("action", "getPolicy")
        .query("model", QWEN_MODEL)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .call()
        .map_err(|e| map_qwen_err(e, "获取上传凭证"))?;
    let p: Value = policy
        .into_json()
        .map_err(|e| format!("上传凭证解析失败：{e}"))?;
    let get = |field: &str| -> Result<String, String> {
        p.pointer(&format!("/data/{field}"))
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| format!("上传凭证缺少 {field}：{}", truncate_for_log(&p)))
    };
    let upload_host = get("upload_host")?;
    let upload_dir = get("upload_dir")?;
    let file_name = format!("asr_{}.mp3", crate::stamp());
    let object_key = format!("{upload_dir}/{file_name}");

    let form: Vec<(String, String)> = vec![
        ("OSSAccessKeyId".into(), get("oss_access_key_id")?),
        ("Signature".into(), get("signature")?),
        ("policy".into(), get("policy")?),
        ("x-oss-object-acl".into(), get("x_oss_object_acl")?),
        ("x-oss-forbid-overwrite".into(), get("x_oss_forbid_overwrite")?),
        ("key".into(), object_key.clone()),
        ("success_action_status".into(), "200".into()),
    ];
    let body = build_multipart(&form, &file_name, audio);
    let resp = agent
        .post(&upload_host)
        .set(
            "Content-Type",
            "multipart/form-data; boundary=lightcapture",
        )
        .send_bytes(&body)
        .map_err(|e| map_qwen_err(e, "上传音频"))?;
    if resp.status() != 200 {
        return Err(format!("上传音频失败：HTTP {}", resp.status()));
    }
    crate::lclog!("Qwen 音频上传完成：{object_key}");
    Ok(format!("oss://{object_key}"))
}

/// 手拼 multipart/form-data（字段固定 7 个 + 一个文件，避免为此引入额外依赖）
pub fn build_multipart(fields: &[(String, String)], file_name: &str, file: &[u8]) -> Vec<u8> {
    let b = "lightcapture";
    let mut out: Vec<u8> = Vec::with_capacity(file.len() + 1024);
    for (k, v) in fields {
        out.extend_from_slice(format!("--{b}\r\n").as_bytes());
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
        );
    }
    out.extend_from_slice(format!("--{b}\r\n").as_bytes());
    out.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(file);
    out.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    out
}

/// 宽松解析 Qwen 转写结果：递归找第一处 `sentences[]`（层级随文档版本变化，
/// 不硬编码路径），再逐句切分成字幕条目。
fn parse_qwen_sentences(v: &Value) -> Vec<Cue> {
    fn find(v: &Value) -> Option<&Vec<Value>> {
        if let Some(arr) = v.get("sentences").and_then(Value::as_array) {
            return Some(arr);
        }
        match v {
            Value::Object(map) => map.values().find_map(find),
            Value::Array(arr) => arr.iter().find_map(find),
            _ => None,
        }
    }
    find(v)
        .map(|arr| arr.iter().flat_map(sentence_to_cues).collect())
        .unwrap_or_default()
}

/// 一句 → 一或多条字幕。Qwen 常把整段语音回成一句（begin–end 跨十几秒），
/// 直接当一条会糊满屏且时间戳对不齐说话时刻；有词级时间戳时按标点切开，
/// 每条的时间戳就是其首末词的发音时刻（项目硬约束：时间戳严格对准说话时刻）。
fn sentence_to_cues(s: &Value) -> Vec<Cue> {
    let words = s.get("words").and_then(Value::as_array);
    let begin = s.get("begin_time").and_then(Value::as_i64);
    let end = s.get("end_time").and_then(Value::as_i64);
    let text = s.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();

    let timed: Vec<(i64, i64, &str, &str)> = words
        .map(|ws| {
            ws.iter()
                .filter_map(|w| {
                    Some((
                        w.get("begin_time")?.as_i64()?,
                        w.get("end_time")?.as_i64()?,
                        w.get("text").and_then(Value::as_str).unwrap_or(""),
                        w.get("punctuation").and_then(Value::as_str).unwrap_or(""),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    // 无词级信息、词为空或有词缺时间戳 → 整句一条
    let whole = |text: String| -> Vec<Cue> {
        match (begin, end, text.is_empty()) {
            (Some(b), Some(e), false) => vec![Cue { start_ms: b, end_ms: e, text }],
            _ => vec![],
        }
    };
    if timed.is_empty() || Some(timed.len()) != words.map(|w| w.len()) {
        return whole(text);
    }

    // 按标点收条目：句末标点必断；逗号类累计 ≥5 字断（避免"好，"式碎条）；
    // 无标点 ≥22 字强制断（防单行过长糊屏）
    const SOFT: [char; 5] = ['，', '、', ',', ':', '：'];
    const HARD: [char; 7] = ['。', '！', '？', '!', '?', '；', ';'];
    let mut cues = Vec::new();
    let mut buf = String::new();
    let mut seg_start = 0i64;
    let mut seg_end = 0i64;
    for (wb, we, wtext, punct) in timed {
        if buf.is_empty() {
            seg_start = wb;
        }
        seg_end = we;
        buf.push_str(wtext);
        buf.push_str(punct);
        let n = buf.chars().count();
        let hard = punct.chars().any(|c| HARD.contains(&c));
        let soft = punct.chars().any(|c| SOFT.contains(&c));
        if hard || (soft && n >= 5) || n >= 22 {
            let t = buf.trim().to_string();
            if !t.is_empty() {
                cues.push(Cue { start_ms: seg_start, end_ms: seg_end, text: t });
            }
            buf.clear();
        }
    }
    let t = buf.trim().to_string();
    if !t.is_empty() {
        cues.push(Cue { start_ms: seg_start, end_ms: seg_end, text: t });
    }
    cues
}

/// 递归找指定键的字符串值（层级随文档版本变化，不硬编码路径）
fn find_str_field(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| map.values().find_map(|x| find_str_field(x, key))),
        Value::Array(arr) => arr.iter().find_map(|x| find_str_field(x, key)),
        _ => None,
    }
}

fn map_qwen_err(e: ureq::Error, step: &str) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let detail = resp.into_string().unwrap_or_default();
            format!("{step}失败：HTTP {code} {detail}")
        }
        other => format!("{step}失败：{other}"),
    }
}

// ================= 连通性测试 =================

/// 测试服务商连通性：只验证网络与凭证，不产生识别费用。
/// - **Qwen**：调 DashScope 上传凭证接口（纯鉴权，不调用模型）
/// - **豆包**：提交 1 秒静音音频；返回 成功/静音/空音频 都证明鉴权通过
///
/// 返回 Ok(描述文案) 或 Err(失败原因)，供设置页「测试连通」按钮展示。
pub fn test_connection() -> Result<String, String> {
    let cfg = crate::settings::get().subtitle;
    match cfg.provider.as_str() {
        "doubao" => test_doubao(&cfg),
        _ => test_qwen(&cfg),
    }
}

fn test_qwen(cfg: &SubtitleCfg) -> Result<String, String> {
    let key = cfg.qwen_api_key.trim();
    if key.is_empty() {
        return Err("请先填入 DashScope API Key 并保存".into());
    }
    let agent = http_agent();
    let resp = agent
        .get(QWEN_UPLOADS)
        .query("action", "getPolicy")
        .query("model", QWEN_MODEL)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .call();
    match resp {
        Ok(_) => Ok(format!("连接正常 · API Key 有效 · 模型 {QWEN_MODEL}")),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            let hint = match code {
                401 | 403 => "API Key 无效或已过期，请核对是否为阿里云百炼的 Key".to_string(),
                _ => detail,
            };
            Err(format!("连接失败：HTTP {code} — {hint}"))
        }
        Err(e) => Err(format!("网络连接失败：{e}")),
    }
}

fn test_doubao(cfg: &SubtitleCfg) -> Result<String, String> {
    let app_key = cfg.doubao_app_key.trim();
    if app_key.is_empty() {
        return Err("请先填入 App Key 并保存".into());
    }
    let body = json!({
        "user": { "uid": app_key },
        "audio": {
            "data": base64::engine::general_purpose::STANDARD.encode(tiny_silence_wav()),
            "format": "wav",
            "rate": 16000,
        },
        "request": { "model_name": "bigmodel" },
    });
    let mut req = http_agent()
        .post(DOUBAO_URL)
        .set("X-Api-App-Key", app_key)
        .set("X-Api-Resource-Id", "volc.bigasr.auc_turbo")
        .set("X-Api-Request-Id", &request_id())
        .set("X-Api-Sequence", "-1");
    let token = cfg.doubao_access_token.trim();
    if !token.is_empty() {
        req = req.set("X-Api-Access-Key", token);
    }
    match req.send_json(body) {
        Ok(resp) => {
            let status = resp.header("X-Api-Status-Code").unwrap_or("");
            match status {
                // 成功识别 / 静音音频 / 空音频：服务端受理并处理了请求 → 鉴权通过
                "20000000" | "20000003" | "45000002" => Ok(
                    "连接正常 · 凭证有效 · 模型 bigmodel 极速版（volc.bigasr.auc_turbo）".into(),
                ),
                other => Err(format!(
                    "连接异常：状态 {other} — {}",
                    resp.header("X-Api-Message").unwrap_or("")
                )),
            }
        }
        Err(ureq::Error::Status(code, r)) => {
            let st = r.header("X-Api-Status-Code").unwrap_or("").to_string();
            let msg = r.header("X-Api-Message").unwrap_or("").to_string();
            let hint = match code {
                401 | 403 => "App Key 或 Access Token 无效，请核对火山控制台凭证".to_string(),
                _ => format!("{st} {msg}"),
            };
            Err(format!("连接失败：HTTP {code} — {hint}"))
        }
        Err(e) => Err(format!("网络连接失败：{e}")),
    }
}

/// 1 秒静音 WAV（16kHz 单声道 16bit），仅用于连通性测试
fn tiny_silence_wav() -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        if let Ok(mut w) = hound::WavWriter::new(&mut cursor, spec) {
            for _ in 0..16_000 {
                if w.write_sample(0i16).is_err() {
                    break;
                }
            }
            let _ = w.finalize();
        }
    }
    cursor.into_inner()
}

// ================= 通用 =================

/// 60s 单请求超时 + 15s 连接超时：绝不让后台线程无限挂起
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .timeout_connect(Duration::from_secs(15))
        .build()
}

/// 伪随机请求 ID（时间戳 + 进程号，服务端仅要求唯一性）
fn request_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    )
}

/// SRT 时间格式：`HH:MM:SS,mmm`
pub fn format_srt_time(ms: i64) -> String {
    let ms = ms.max(0);
    let (h, m, s, mm) = (
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000,
    );
    format!("{h:02}:{m:02}:{s:02},{mm:03}")
}

/// 生成 SRT 文本
pub fn build_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            format_srt_time(c.start_ms),
            format_srt_time(c.end_ms),
            c.text.trim()
        ));
    }
    out
}

/// 响应日志截断（避免超长 JSON 撑爆日志）
fn truncate_for_log(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 1500 {
        format!("{}…(共 {} 字节)", &s[..1500], s.len())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_time_formatting() {
        assert_eq!(format_srt_time(0), "00:00:00,000");
        assert_eq!(format_srt_time(1530), "00:00:01,530");
        assert_eq!(format_srt_time(3_723_456), "01:02:03,456");
        assert_eq!(format_srt_time(-5), "00:00:00,000", "负数夹到 0");
    }

    #[test]
    fn build_srt_shape() {
        let cues = vec![
            Cue { start_ms: 450, end_ms: 1530, text: "第一句。".into() },
            Cue { start_ms: 1530, end_ms: 3000, text: "  第二句。 ".into() },
        ];
        let srt = build_srt(&cues);
        let expected = "1\n00:00:00,450 --> 00:00:01,530\n第一句。\n\n\
                        2\n00:00:01,530 --> 00:00:03,000\n第二句。\n\n";
        assert_eq!(srt, expected, "SRT 格式：编号/时间轴/文本/空行，文本去首尾空白");
    }

    #[test]
    fn burn_args_hardcode_subs_and_copy_audio() {
        let args = build_burn_args("C:\\v\\a.mp4", "C:\\tmp\\sub.srt", "C:\\v\\out.mp4");
        let joined = args.join(" ");
        assert!(joined.contains("subtitles="), "字幕烧录进画面（硬字幕）");
        assert!(joined.contains("libx264"), "画面必须重编码才能烧字幕");
        assert!(joined.contains("-c:a copy"), "音轨不重编码（轻量约束）");
        assert!(joined.contains("FontName=Microsoft YaHei"), "中文字体");
        // 滤镜路径转义：反斜杠→正斜杠、冒号→\:，否则盘符被截断
        assert!(joined.contains("C\\:/tmp/sub.srt"), "路径转义：{joined}");
        assert!(args.last().unwrap() == "C:\\v\\out.mp4", "输出落临时文件，替换原片由调用方处理");
    }

    #[test]
    fn parses_doubao_response() {
        let v = json!({
            "audio_info": { "duration": 2499 },
            "result": {
                "text": "关闭透传。",
                "utterances": [
                    {
                        "start_time": 450,
                        "end_time": 1530,
                        "text": "关闭透传。",
                        "words": [{ "text": "关", "start_time": 450, "end_time": 770 }]
                    },
                    { "start_time": 1600, "end_time": 2400, "text": "   " },
                    { "text": "缺时间戳" }
                ]
            }
        });
        let cues = parse_doubao(&v);
        assert_eq!(cues.len(), 1, "空文本与缺时间戳的句应被跳过");
        assert_eq!(cues[0].start_ms, 450);
        assert_eq!(cues[0].end_ms, 1530);
        assert_eq!(cues[0].text, "关闭透传。");

        assert!(parse_doubao(&json!({"result": {}})).is_empty(), "无 utterances 返回空");
    }

    #[test]
    fn parses_qwen_sentences_flexibly() {
        // 真实层级可能嵌在 output.result 下，宽松递归查找
        let v = json!({
            "output": {
                "task_status": "SUCCEEDED",
                "result": {
                    "sentences": [
                        { "begin_time": 0, "end_time": 1200, "text": "你好世界。" },
                        { "begin_time": 1200, "end_time": 2400, "text": "第二句" }
                    ]
                }
            }
        });
        let cues = parse_qwen_sentences(&v);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ms, 0);
        assert_eq!(cues[1].end_ms, 2400);
        assert_eq!(cues[1].text, "第二句");

        assert!(
            parse_qwen_sentences(&json!({"output": {"task_status": "SUCCEEDED"}})).is_empty(),
            "无 sentences 返回空"
        );
    }

    #[test]
    fn qwen_words_split_long_sentence_at_punctuation() {
        // 真实场景：整段语音只回一句，靠词级时间戳+标点切成多条
        let v = json!({
            "transcripts": [{
                "sentences": [{
                    "begin_time": 1700,
                    "end_time": 5000,
                    "text": "今天天气不错，我们出去走走吧。好的马上。",
                    "words": [
                        { "begin_time": 1700, "end_time": 2000, "text": "今天", "punctuation": "" },
                        { "begin_time": 2000, "end_time": 2300, "text": "天气", "punctuation": "" },
                        { "begin_time": 2300, "end_time": 2500, "text": "不错", "punctuation": "，" },
                        { "begin_time": 2500, "end_time": 2800, "text": "我们", "punctuation": "" },
                        { "begin_time": 2800, "end_time": 3000, "text": "出去", "punctuation": "" },
                        { "begin_time": 3000, "end_time": 3300, "text": "走走吧", "punctuation": "。" },
                        { "begin_time": 3300, "end_time": 3500, "text": "好的", "punctuation": "" },
                        { "begin_time": 3500, "end_time": 3800, "text": "马上", "punctuation": "。" }
                    ]
                }]
            }]
        });
        let cues = parse_qwen_sentences(&v);
        assert_eq!(cues.len(), 3, "一句长话按标点切成 3 条");
        assert_eq!(cues[0].text, "今天天气不错，");
        assert_eq!((cues[0].start_ms, cues[0].end_ms), (1700, 2500), "时间戳=首末词发音时刻");
        assert_eq!(cues[1].text, "我们出去走走吧。");
        assert_eq!((cues[1].start_ms, cues[1].end_ms), (2500, 3300));
        assert_eq!(cues[2].text, "好的马上。");
    }

    #[test]
    fn qwen_sentence_without_words_stays_whole() {
        let v = json!({
            "sentences": [
                { "begin_time": 0, "end_time": 1200, "text": "你好世界。" },
                { "begin_time": 1200, "end_time": 2400, "text": "第二句" }
            ]
        });
        let cues = parse_qwen_sentences(&v);
        assert_eq!(cues.len(), 2, "无词级信息整句一条");
        assert_eq!(cues[0].end_ms, 1200);
    }

    #[test]
    fn finds_transcription_url_anywhere() {
        // 真实响应：结果不内联，只在 output.result.transcription_url 给外链
        let v = json!({
            "output": {
                "task_status": "SUCCEEDED",
                "result": { "transcription_url": "https://oss.example/x.json?sig=1" }
            }
        });
        assert_eq!(
            find_str_field(&v, "transcription_url").as_deref(),
            Some("https://oss.example/x.json?sig=1"),
            "递归找外链字段"
        );
        assert!(find_str_field(&json!({"output": {}}), "transcription_url").is_none());
    }

    #[test]
    fn multipart_has_all_fields_and_binary() {
        let fields = vec![("key".to_string(), "dir/a.mp3".to_string())];
        let body = build_multipart(&fields, "a.mp3", &[0xFF, 0xD8, 0x00, 0x0A]);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"key\""));
        assert!(text.contains("dir/a.mp3"));
        assert!(text.contains("filename=\"a.mp3\""));
        assert!(text.starts_with("--lightcapture\r\n"));
        assert!(text.ends_with("--lightcapture--\r\n"));
        // 二进制内容原样保留（0x0A 是 LF，非 UTF-8 可打印但保留）
        assert!(body.windows(4).any(|w| w == [0xFF, 0xD8, 0x00, 0x0A]));
    }

    #[test]
    fn subtitle_not_usable_without_creds() {
        // 未配置 → generate 第一步就短路（这里只测判定逻辑）
        let cfg = SubtitleCfg::default();
        assert!(!cfg.usable());
        let cfg = SubtitleCfg {
            enabled: true,
            provider: "qwen".into(),
            ..SubtitleCfg::default()
        };
        assert!(!cfg.usable(), "开关开着但没 key 也不可用");
    }
}
