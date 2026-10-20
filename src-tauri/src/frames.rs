//! 关键帧、拼图与时间轴：把一段录屏变成 Agent 能直接理解的结构化信息。
//!
//! 为什么需要：多模态模型看视频会严重降采样，「样式对不对、细节有没有问题」
//! 往往不如几张全分辨率静帧可靠；而流程与节奏又只有视频能表达。所以提供两条互补的
//! 输出——视频看流程、关键帧看细节。
//!
//! 时间轴（timeline）给出：时长、分辨率、帧率、场景切换点与切分后的片段区间。
//! 这些是纯元数据，Agent 可以先读它，再决定抽哪几帧、看哪一段，避免盲抽。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// 视频基础信息
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MediaInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// 场景切换点
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneCut {
    /// 秒
    pub t: f64,
    /// 变化强度 0~1
    pub score: f64,
}

/// 时间轴片段（相邻切换点之间）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
}

impl Segment {
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}

/// 解析 `ffmpeg -i` 的 stderr，取时长、分辨率、帧率。
/// 只认第一条 `Video:` 流，避免多流文件里的封面图/字幕流干扰。
pub fn parse_media_info(stderr: &str) -> Result<MediaInfo, String> {
    let mut duration = None;
    let mut size = None;
    let mut fps = None;
    for line in stderr.lines() {
        if duration.is_none() {
            if let Some(rest) = line.trim().strip_prefix("Duration:") {
                if let Some(v) = parse_hms(rest.trim()) {
                    duration = Some(v);
                }
            }
        }
        if size.is_none() && line.contains("Video:") {
            if let Some((w, h)) = parse_size(line) {
                size = Some((w, h));
                fps = parse_fps(line);
            }
        }
        if duration.is_some() && size.is_some() && fps.is_some() {
            break;
        }
    }
    let (width, height) =
        size.ok_or_else(|| "无法识别视频分辨率（可能不是视频文件）".to_string())?;
    let duration = duration.ok_or_else(|| "无法读取视频时长".to_string())?;
    Ok(MediaInfo {
        duration,
        width,
        height,
        // 少数容器不报 fps，按 30 兜底——它只影响建议采样，不影响实际抽帧
        fps: fps.unwrap_or(30.0),
    })
}

/// `00:01:23.45` / `00:01:23.45, start: ...` → 秒
fn parse_hms(s: &str) -> Option<f64> {
    let head = s.split(',').next()?.trim();
    let mut parts = head.split(':');
    let h: f64 = parts.next()?.trim().parse().ok()?;
    let m: f64 = parts.next()?.trim().parse().ok()?;
    let sec: f64 = parts.next()?.trim().trim_end_matches(',').parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(h * 3600.0 + m * 60.0 + sec)
}

/// 从 `..., 640x360 [SAR 1:1 ...]` 里取分辨率；跳过 `0x31637661` 这类编码标签。
fn parse_size(line: &str) -> Option<(u32, u32)> {
    for token in line.split(|c: char| c.is_whitespace() || c == ',' || c == '[') {
        let token = token.trim();
        let Some((w, h)) = token.split_once('x') else {
            continue;
        };
        let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) else {
            continue;
        };
        // 编码 tag（如 0x31637661）会被误判；真实分辨率不会小到离谱
        if (16..=16384).contains(&w) && (16..=16384).contains(&h) {
            return Some((w, h));
        }
    }
    None
}

/// 从 `..., 30 fps, 30 tbr, ...` 里取帧率；也兼容 `29.97 fps`
fn parse_fps(line: &str) -> Option<f64> {
    for token in line.split(',') {
        let t = token.trim();
        if let Some(num) = t.strip_suffix("fps") {
            if let Ok(v) = num.trim().parse::<f64>() {
                if v > 0.0 && v < 1000.0 {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// 解析场景检测输出：
/// `[Parsed_metadata_1 @ ...] frame:0 pts:30720 pts_time:2`
/// `[Parsed_metadata_1 @ ...] lavfi.scene_score=1.000000`
pub fn parse_scene_cuts(stderr: &str) -> Vec<SceneCut> {
    let mut out = Vec::new();
    let mut pending: Option<f64> = None;
    for line in stderr.lines() {
        if let Some(idx) = line.find("pts_time:") {
            let rest = &line[idx + "pts_time:".len()..];
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect();
            pending = num.parse::<f64>().ok();
        } else if line.contains("lavfi.scene_score=") {
            if let (Some(t), Some(score)) = (pending.take(), line.split('=').next_back()) {
                if let Ok(score) = score.trim().parse::<f64>() {
                    out.push(SceneCut { t, score });
                }
            }
        }
    }
    out
}

/// 把切换点转成连续片段。切换点会被夹进 (0, duration) 且去重排序；
/// 过密的切换（同一帧附近重复）只保留第一个。
pub fn segments_from_cuts(duration: f64, cuts: &[SceneCut]) -> Vec<Segment> {
    let mut times: Vec<f64> = cuts
        .iter()
        .map(|c| c.t)
        .filter(|t| *t > 0.05 && *t < duration - 0.05)
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    let mut segs = Vec::with_capacity(times.len() + 1);
    let mut start = 0.0;
    for t in times {
        segs.push(Segment { start, end: t });
        start = t;
    }
    segs.push(Segment {
        start,
        end: duration,
    });
    segs
}

/// 均匀采样时刻：取每段的中心，避免首帧黑场/淡入和末帧不完整。
/// count 会被夹到 1..=120（再多对 Agent 没有意义，只是浪费 token）。
pub fn sample_times(duration: f64, count: u32) -> Vec<f64> {
    if duration <= 0.0 {
        return Vec::new();
    }
    let count = count.clamp(1, 120) as usize;
    (0..count)
        .map(|i| {
            let t = (i as f64 + 0.5) * duration / count as f64;
            // 浮点误差可能落到片尾之外，留 1ms 余量
            t.min((duration - 0.001).max(0.0))
        })
        .collect()
}

/// 探测视频信息（`ffmpeg -i` 会把信息写在 stderr）。
///
/// 注意：`ffmpeg -i` 不带输出文件时**固定返回退出码 1**（"At least one output file
/// must be specified"），这是正常行为，所以这里以 stderr 能否解析出信息为准，
/// 不看退出码。
pub fn probe(input: &str) -> Result<MediaInfo, String> {
    // 先确认 ffmpeg 存在：否则会报"ffmpeg 退出码 ..."这种让人摸不着头脑的错误
    crate::ffmpeg::locate()?;
    let args = vec![
        "-hide_banner".to_string(),
        "-i".to_string(),
        input.to_string(),
    ];
    let (_, stderr) = run_capture_impl(&args, Duration::from_secs(30), false)?;
    parse_media_info(&stderr)
}

/// 场景切换检测。threshold 越大越只保留明显变化。
pub fn scene_cuts(input: &str, threshold: f64) -> Result<Vec<SceneCut>, String> {
    let threshold = threshold.clamp(0.05, 0.95);
    let filter = format!("select='gt(scene,{threshold:.2})',metadata=print");
    let args = vec![
        "-hide_banner".to_string(),
        "-nostats".to_string(),
        "-i".to_string(),
        input.to_string(),
        "-vf".to_string(),
        filter,
        "-an".to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    let (_, stderr) = run_capture(&args, Duration::from_secs(300))?;
    Ok(parse_scene_cuts(&stderr))
}

/// 抽单帧参数。`-ss` 放在 `-i` 前走快速定位；抽静帧不需要帧级精确。
pub fn build_frame_args(input: &str, t: f64, out: &Path, max_width: Option<u32>) -> Vec<String> {
    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-nostats".to_string(),
        "-ss".to_string(),
        format!("{t:.3}"),
        "-i".to_string(),
        input.to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
    ];
    if let Some(w) = max_width {
        args.push("-vf".into());
        args.push(format!("scale={}:-2:flags=lanczos", w & !1));
    }
    args.push("-q:v".into());
    args.push("2".into());
    args.push(out.to_string_lossy().into_owned());
    args
}

/// 拼图参数：均匀取 count 帧 → 缩放 → 平铺成一张图。
/// 用 `fps` 过滤器一次解码完成，比逐个 seek 快得多。
pub fn build_sheet_args(
    input: &str,
    out: &Path,
    duration: f64,
    count: u32,
    cols: u32,
    max_width: Option<u32>,
) -> Vec<String> {
    let count = count.clamp(1, 120);
    let cols = cols.clamp(1, 12);
    let rows = count.div_ceil(cols).max(1);
    let fps = if duration > 0.0 {
        count as f64 / duration
    } else {
        1.0
    };
    let mut vf = format!("fps={fps:.6}");
    if let Some(w) = max_width {
        vf.push_str(&format!(",scale={}:-2:flags=bilinear", w & !1));
    }
    vf.push_str(&format!(",tile={cols}x{rows}:padding=6:margin=6"));
    vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-nostats".to_string(),
        "-i".to_string(),
        input.to_string(),
        "-vf".to_string(),
        vf,
        "-frames:v".to_string(),
        "1".to_string(),
        "-q:v".to_string(),
        "2".to_string(),
        out.to_string_lossy().into_owned(),
    ]
}

/// 抽帧到目录，文件名为 `frame_000.png`（或 .jpg）。
pub fn extract_frames(
    input: &str,
    out_dir: &Path,
    times: &[f64],
    max_width: Option<u32>,
    ext: &str,
) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("创建目录失败：{e}"))?;
    let mut saved = Vec::with_capacity(times.len());
    for (i, t) in times.iter().enumerate() {
        let path = out_dir.join(format!("frame_{i:03}.{ext}"));
        let args = build_frame_args(input, *t, &path, max_width);
        run_capture(&args, Duration::from_secs(60))
            .map_err(|e| format!("抽取第 {i} 帧（{t:.3}s）失败：{e}"))?;
        if !path.exists() {
            return Err(format!("第 {i} 帧未生成（{t:.3}s 可能超出片长）"));
        }
        saved.push(path);
    }
    Ok(saved)
}

/// 生成拼图单图。
pub fn contact_sheet(
    input: &str,
    out: &Path,
    duration: f64,
    count: u32,
    cols: u32,
    max_width: Option<u32>,
) -> Result<(), String> {
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
        }
    }
    let args = build_sheet_args(input, out, duration, count, cols, max_width);
    run_capture(&args, Duration::from_secs(180)).map_err(|e| format!("生成拼图失败：{e}"))?;
    if !out.exists() {
        return Err("拼图未生成".into());
    }
    Ok(())
}

/// 执行 ffmpeg 并捕获输出（stdout/stderr 都读走，避免管道写满导致死锁），
/// 带超时强杀——长视频场景检测可能跑很久，不能无限等。
fn run_capture(args: &[String], timeout: Duration) -> Result<(String, String), String> {
    run_capture_impl(args, timeout, true)
}

/// `check_status=false` 用于 `ffmpeg -i` 这类"正常也会返回 1"的探测调用。
fn run_capture_impl(
    args: &[String],
    timeout: Duration,
    check_status: bool,
) -> Result<(String, String), String> {
    use std::io::Read;

    let mut child = crate::ffmpeg::command()?
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 ffmpeg：{e}"))?;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    match status {
        Some(s) if s.success() || !check_status => Ok((stdout, stderr)),
        Some(s) => Err(format!(
            "ffmpeg 退出码 {:?}：{}",
            s.code(),
            tail(&stderr, 800)
        )),
        None => Err("ffmpeg 执行超时".into()),
    }
}

/// 错误信息只保留尾部，避免把整段 ffmpeg 日志塞进 JSON
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    let start = s
        .char_indices()
        .map(|(i, _)| i)
        .find(|i| *i >= start)
        .unwrap_or(s.len());
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: &str = r#"
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'demo.mp4':
  Duration: 00:01:23.45, start: 0.000000, bitrate: 1200 kb/s
  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 1920x1080 [SAR 1:1 DAR 16:9], 30 fps, 30 tbr, 15360 tbn
"#;

    #[test]
    fn parses_media_info() {
        let info = parse_media_info(PROBE).unwrap();
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert!((info.duration - 83.45).abs() < 0.01);
        assert!((info.fps - 30.0).abs() < 0.01);
    }

    #[test]
    fn parses_fractional_fps() {
        let s = "  Stream #0:0: Video: h264, yuv420p, 1280x720 [SAR 1:1 DAR 16:9], 29.97 fps, 29.97 tbr";
        let info = parse_media_info(&format!("Duration: 00:00:10.00, start: 0\n{s}")).unwrap();
        assert!((info.fps - 29.97).abs() < 0.01);
        assert_eq!((info.width, info.height), (1280, 720));
    }

    #[test]
    fn rejects_non_video() {
        assert!(parse_media_info("some random text\n").is_err());
    }

    #[test]
    fn parses_scene_cuts() {
        let s = r#"
[Parsed_metadata_1 @ 0x55] frame:0    pts:30720   pts_time:2
[Parsed_metadata_1 @ 0x55] lavfi.scene_score=1.000000
[Parsed_metadata_1 @ 0x55] frame:1    pts:61440   pts_time:4.5
[Parsed_metadata_1 @ 0x55] lavfi.scene_score=0.650000
"#;
        let cuts = parse_scene_cuts(s);
        assert_eq!(cuts.len(), 2);
        assert!((cuts[0].t - 2.0).abs() < 0.001);
        assert!((cuts[0].score - 1.0).abs() < 0.001);
        assert!((cuts[1].t - 4.5).abs() < 0.001);
    }

    #[test]
    fn segments_cover_whole_video() {
        let cuts = vec![
            SceneCut { t: 2.0, score: 1.0 },
            SceneCut { t: 4.0, score: 0.7 },
        ];
        let segs = segments_from_cuts(6.0, &cuts);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].start, 0.0);
        assert!((segs[0].end - 2.0).abs() < 0.001);
        assert!((segs[2].end - 6.0).abs() < 0.001);
        let total: f64 = segs.iter().map(|s| s.duration()).sum();
        assert!((total - 6.0).abs() < 0.01, "片段应无缝覆盖整段");
    }

    #[test]
    fn segments_ignore_edge_and_duplicate_cuts() {
        let cuts = vec![
            SceneCut { t: 0.0, score: 1.0 },
            SceneCut { t: 2.0, score: 1.0 },
            SceneCut {
                t: 2.02,
                score: 1.0,
            },
            SceneCut { t: 6.0, score: 1.0 },
        ];
        let segs = segments_from_cuts(6.0, &cuts);
        assert_eq!(segs.len(), 2, "边缘与重复切换点应被忽略");
    }

    #[test]
    fn sample_times_are_centered_and_bounded() {
        let ts = sample_times(10.0, 4);
        assert_eq!(ts.len(), 4);
        assert!((ts[0] - 1.25).abs() < 0.001, "首帧取首段中心");
        assert!((ts[3] - 8.75).abs() < 0.001);
        assert!(ts.iter().all(|t| *t >= 0.0 && *t < 10.0));
    }

    #[test]
    fn sample_times_clamped() {
        assert_eq!(sample_times(10.0, 0).len(), 1, "count 至少 1");
        assert_eq!(sample_times(10.0, 9999).len(), 120, "上限 120");
        assert!(sample_times(0.0, 5).is_empty());
    }

    #[test]
    fn frame_args_seek_before_input() {
        let args = build_frame_args("in.mp4", 1.5, Path::new("f.png"), Some(1281));
        let ss = args.iter().position(|a| a == "-ss").unwrap();
        let i = args.iter().position(|a| a == "-i").unwrap();
        assert!(ss < i, "-ss 应在 -i 前（快速定位）");
        assert_eq!(args[ss + 1], "1.500");
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert_eq!(
            args[vf + 1],
            "scale=1280:-2:flags=lanczos",
            "奇数宽度应规整"
        );
        assert_eq!(args.last().unwrap(), "f.png");
    }

    #[test]
    fn sheet_args_tile_grid() {
        let args = build_sheet_args("in.mp4", Path::new("s.png"), 6.0, 10, 4, Some(1600));
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        let filter = &args[vf + 1];
        assert!(
            filter.contains("tile=4x3"),
            "10 张按 4 列应排成 3 行：{filter}"
        );
        assert!(filter.contains("scale=1600:-2"));
        assert!(filter.contains("fps=1.666667"));
        assert_eq!(args.last().unwrap(), "s.png");
    }

    #[test]
    fn sheet_single_row() {
        let args = build_sheet_args("in.mp4", Path::new("s.png"), 4.0, 3, 3, None);
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert!(args[vf + 1].contains("tile=3x1"));
    }
}
