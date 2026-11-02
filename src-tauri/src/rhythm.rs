//! 节奏分析：音频 onset 表 + 逐帧运动能量 + 节拍对齐度。
//!
//! 给"看片的模型"一份机读的节奏数据：这视频踩没踩点、动得够不够、
//! 动的瞬间和鼓点差多少——用于判断"有没有节奏感/律动感"，以及
//! 复刻/生成视频后与参考片做量化对比。
//!
//! 实现只依赖 ffmpeg 与本文件自实现的 FFT/统计，不引入任何新 crate：
//! - onset：解码单声道 22.05kHz PCM → 1024 点 Hann 窗 FFT → 频谱通量 →
//!   z-score 峰值拾取（阈值 1.0、最小间隔 0.10s）；
//! - 运动能量：缩放为 160x90 灰度 → 逐帧绝对差均值（流式，不整段载入）；
//! - 对齐度：运动峰值 / onset 互相落在 ±0.12s 内的比例。

use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SR: usize = 22050; // 解码采样率
const WIN: usize = 1024; // 分析窗
const HOP: usize = 256; // 帧移
const FLUX_THRESHOLD: f64 = 1.0; // 通量 z-score 阈值
const MIN_GAP: f64 = 0.10; // onset 最小间隔（秒）
const SYNC_WINDOW: f64 = 0.12; // 对齐判定窗口（秒）
const GRAY_W: usize = 160; // 运动能量分析分辨率
const GRAY_H: usize = 90;

/// 跑 ffmpeg，返回原始字节 stdout（rawvideo / f32le 这类二进制流）。
/// 与 frames::run_capture 同思路，但 stdout 必须保持字节原样。
fn run_bytes(args: &[String], timeout: Duration) -> Result<Vec<u8>, String> {
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
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("等待 ffmpeg 失败：{e}")),
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).to_string();
    match status {
        Some(s) if s.success() => Ok(stdout),
        Some(s) => Err(format!(
            "ffmpeg 退出码 {s}：{}",
            stderr.lines().rev().take(3).collect::<Vec<_>>().join(" / ")
        )),
        None => Err("ffmpeg 超时".into()),
    }
}

/// 解码为单声道 f32 PCM（22.05kHz）。没有音轨的视频会失败——由调用方降级为空 onset。
fn decode_pcm(input: &str) -> Result<Vec<f32>, String> {
    let args = vec![
        "-hide_banner".to_string(),
        "-nostats".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input.to_string(),
        "-vn".to_string(),
        "-ac".to_string(),
        "1".to_string(),
        "-ar".to_string(),
        SR.to_string(),
        "-f".to_string(),
        "f32le".to_string(),
        "-".to_string(),
    ];
    let bytes = run_bytes(&args, Duration::from_secs(300))?;
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// 就地迭代 radix-2 FFT（复数交错存于 re/im 两个数组）
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());
    // 位反转置换
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    // 蝶形运算
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let half = len / 2;
        let mut i = 0;
        while i < n {
            let (mut cur_r, mut cur_i) = (1.0f32, 0.0f32);
            for k in 0..half {
                let (ur, ui) = (re[i + k], im[i + k]);
                let (vr, vi) = (
                    re[i + k + half] * cur_r - im[i + k + half] * cur_i,
                    re[i + k + half] * cur_i + im[i + k + half] * cur_r,
                );
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + half] = ur - vr;
                im[i + k + half] = ui - vi;
                let nr = cur_r * wr - cur_i * wi;
                cur_i = cur_r * wi + cur_i * wr;
                cur_r = nr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// 频谱通量峰值拾取 → onset 时间表（秒）
fn onset_table(pcm: &[f32]) -> Vec<f64> {
    if pcm.len() < WIN * 3 {
        return Vec::new();
    }
    let hann: Vec<f32> = (0..WIN)
        .map(|i| {
            (std::f32::consts::PI * i as f32 / (WIN - 1) as f32)
                .sin()
                .powi(2)
        })
        .collect();
    let mut flux: Vec<f64> = Vec::with_capacity((pcm.len() - WIN) / HOP + 1);
    let mut prev: Vec<f32> = Vec::new();
    let mut re = vec![0f32; WIN];
    let mut im = vec![0f32; WIN];
    let mut start = 0usize;
    while start + WIN <= pcm.len() {
        for i in 0..WIN {
            re[i] = pcm[start + i] * hann[i];
            im[i] = 0.0;
        }
        fft(&mut re, &mut im);
        let mags: Vec<f32> = (0..WIN / 2)
            .map(|i| (re[i] * re[i] + im[i] * im[i]).sqrt())
            .collect();
        if prev.is_empty() {
            prev = mags;
            flux.push(0.0);
        } else {
            let s: f32 = mags
                .iter()
                .zip(&prev)
                .map(|(a, b)| (*a - *b).max(0.0))
                .sum();
            flux.push(s as f64);
            prev = mags;
        }
        start += HOP;
    }
    // z-score 归一化
    let mean = flux.iter().sum::<f64>() / flux.len() as f64;
    let var = flux.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / flux.len() as f64;
    let std = var.sqrt().max(1e-12);
    // 峰值拾取：高于阈值的局部极大，且与上一个保持最小间隔
    let mut peaks = Vec::new();
    for i in 2..flux.len().saturating_sub(2) {
        let z = (flux[i] - mean) / std;
        if z > FLUX_THRESHOLD && z >= (flux[i - 1] - mean) / std && z > (flux[i + 1] - mean) / std
        {
            let t = i as f64 * HOP as f64 / SR as f64;
            if peaks
                .last()
                .map(|p: &f64| t - *p >= MIN_GAP)
                .unwrap_or(true)
            {
                peaks.push(t);
            }
        }
    }
    peaks
}

/// BPM 估计：在 60~180 内搜使 onset 落点最多的等间隔网格（0.5 BPM 步长、0.02s 相位）
fn estimate_bpm(onsets: &[f64]) -> Option<f64> {
    if onsets.len() < 8 {
        return None;
    }
    let mut best: Option<(usize, f64)> = None;
    let mut bpm = 60.0;
    while bpm <= 180.0 {
        let period = 60.0 / bpm;
        let mut phase = 0.0;
        while phase < period {
            let count = onsets
                .iter()
                .filter(|t| {
                    let d = ((**t - phase + period / 2.0).rem_euclid(period) - period / 2.0).abs();
                    d < 0.05
                })
                .count();
            if best.map(|(c, _)| count > c).unwrap_or(true) {
                best = Some((count, bpm));
            }
            phase += 0.02;
        }
        bpm += 0.5;
    }
    best.map(|(_, b)| (b * 10.0).round() / 10.0)
}

/// 逐帧运动能量：160x90 灰度帧间绝对差均值。流式读取，内存只留两帧。
fn motion_energy(input: &str) -> Result<Vec<f64>, String> {
    let frame = GRAY_W * GRAY_H;
    let args = vec![
        "-hide_banner".to_string(),
        "-nostats".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input.to_string(),
        "-vf".to_string(),
        format!("scale={GRAY_W}:{GRAY_H},format=gray"),
        "-f".to_string(),
        "rawvideo".to_string(),
        "-".to_string(),
    ];
    let mut child = crate::ffmpeg::command()?
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 ffmpeg：{e}"))?;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let mut energy: Vec<f64> = Vec::new();
    let mut prev: Option<Vec<u8>> = None;
    let mut data: Vec<u8> = Vec::with_capacity(frame * 64);
    let mut pos = 0usize;
    let mut buf = vec![0u8; frame * 64];
    let stdout = match out_pipe.as_mut() {
        Some(p) => p,
        None => return Err("ffmpeg stdout 不可用".into()),
    };
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("运动能量分析超时".into());
        }
        let n = stdout
            .read(&mut buf)
            .map_err(|e| format!("读取 ffmpeg 输出失败：{e}"))?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        while data.len() - pos >= frame {
            let cur = &data[pos..pos + frame];
            if let Some(p) = prev.as_mut() {
                let sum: u32 = cur
                    .iter()
                    .zip(p.iter())
                    .map(|(a, b)| (*a as i16 - *b as i16).unsigned_abs() as u32)
                    .sum();
                energy.push(sum as f64 / frame as f64);
            }
            prev = Some(cur.to_vec());
            pos += frame;
        }
        if pos > frame * 32 {
            data.drain(..pos);
            pos = 0;
        }
    }
    let status = child.wait().map_err(|e| format!("等待 ffmpeg 失败：{e}"))?;
    let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).to_string();
    if !status.success() {
        return Err(format!(
            "ffmpeg 退出码 {status}：{}",
            stderr.lines().rev().take(3).collect::<Vec<_>>().join(" / ")
        ));
    }
    if energy.is_empty() {
        return Err("视频没有可分析的画面帧".into());
    }
    Ok(energy)
}

/// 运动能量统计：均值、峰值、峰值时刻、逐秒均值
fn motion_stats(energy: &[f64], fps: f64) -> (f64, f64, Vec<f64>, Vec<f64>) {
    let n = energy.len() as f64;
    let mean = energy.iter().sum::<f64>() / n;
    let var = energy.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let thr = mean + var.sqrt();
    let fps = if fps > 0.0 { fps } else { 24.0 };
    let mut peaks = Vec::new();
    for i in 1..energy.len().saturating_sub(1) {
        if energy[i] > thr && energy[i] >= energy[i - 1] && energy[i] > energy[i + 1] {
            peaks.push(round2((i + 1) as f64 / fps));
        }
    }
    let max = energy.iter().cloned().fold(0.0f64, f64::max);
    let secs = ((energy.len() as f64 / fps).ceil() as usize).max(1);
    let mut per_second = Vec::with_capacity(secs);
    for s in 0..secs {
        let lo = (s as f64 * fps) as usize;
        let hi = (((s + 1) as f64 * fps) as usize).min(energy.len());
        if hi > lo {
            let sum: f64 = energy[lo..hi].iter().sum();
            per_second.push(round2(sum / (hi - lo) as f64));
        } else {
            per_second.push(0.0);
        }
    }
    (round2(mean), round2(max), peaks, per_second)
}

/// 对齐度：运动峰值落在 onset ±窗口内的比例、onset 被运动命中的比例
fn alignment(peaks: &[f64], onsets: &[f64]) -> (f64, f64) {
    if peaks.is_empty() || onsets.is_empty() {
        return (0.0, 0.0);
    }
    let near = |a: &[f64], b: &[f64]| {
        a.iter()
            .filter(|x| b.iter().any(|y| (*x - *y).abs() <= SYNC_WINDOW))
            .count() as f64
            / a.len() as f64
    };
    (round2(near(peaks, onsets)), round2(near(onsets, peaks)))
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 节奏分析入口：永不失败，缺数据的字段以 error 说明降级。
/// `fps` 来自已有的 probe 结果，用于把帧序号换算成秒。
pub fn rhythm_json(input: &str, fps: f64) -> Value {
    let (pcm, audio_err) = match decode_pcm(input) {
        Ok(p) => (p, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let onsets = onset_table(&pcm);
    let bpm = estimate_bpm(&onsets);
    let ioi: Vec<f64> = onsets.windows(2).map(|w| round2(w[1] - w[0])).collect();
    let mut median_ioi = ioi.clone();
    median_ioi.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median_ioi = median_ioi
        .get(median_ioi.len() / 2)
        .copied()
        .map(|v| round2(v * 1000.0) / 1000.0);

    let audio = json!({
        "onsets": onsets.iter().map(|t| round2(*t)).collect::<Vec<_>>(),
        "onset_count": onsets.len(),
        "bpm": bpm,
        "median_ioi_s": median_ioi,
        "error": audio_err,
    });

    match motion_energy(input) {
        Ok(energy) => {
            let (mean, max, peaks, per_second) = motion_stats(&energy, fps);
            let (peak_hit, onset_hit) = alignment(&peaks, &onsets);
            json!({
                "audio": audio,
                "motion": {
                    "mean": mean,
                    "max": max,
                    "peaks": peaks,
                    "peak_count": peaks.len(),
                    "per_second": per_second,
                    "energy": energy.iter().map(|v| round2(*v)).collect::<Vec<_>>(),
                },
                "sync": {
                    "window_s": SYNC_WINDOW,
                    "peak_to_onset": peak_hit,
                    "onset_to_motion": onset_hit,
                },
            })
        }
        Err(e) => json!({
            "audio": audio,
            "motion": { "error": e },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已知时刻的点击声 → onset 应落在网格上
    #[test]
    fn onset_recovers_click_train() {
        let sr = SR as f32;
        let mut pcm = vec![0f32; sr as usize * 6];
        // 每 0.5s 一个 10ms 的 2kHz 正弦点击
        for k in 0..12 {
            let start = (k as f32 * 0.5 * sr) as usize;
            let len = (0.01 * sr) as usize;
            for i in 0..len {
                let env = (std::f32::consts::PI * i as f32 / len as f32).sin();
                let v = (2.0 * std::f32::consts::PI * 2000.0 * i as f32 / sr).sin() * env;
                if start + i < pcm.len() {
                    pcm[start + i] += v;
                }
            }
        }
        let onsets = onset_table(&pcm);
        assert!(onsets.len() >= 8, "onset 太少：{onsets:?}");
        let hits = onsets
            .iter()
            .filter(|t| {
                let grid = (**t / 0.5).round() * 0.5;
                (*t - grid).abs() < 0.06
            })
            .count();
        assert!(
            hits as f64 / onsets.len() as f64 > 0.8,
            "onset 未对齐 0.5s 网格：{onsets:?}"
        );
    }

    /// 稀疏尖峰的能量曲线 → 峰值时刻应被找回来
    #[test]
    fn peaks_on_sparse_spikes() {
        let mut energy = vec![0.05f64; 480];
        energy[119] = 8.0;
        energy[239] = 12.0;
        let (mean, max, peaks, per_second) = motion_stats(&energy, 24.0);
        assert!(max >= 11.9);
        assert!(mean > 0.05);
        assert_eq!(peaks, vec![5.0, 10.0], "峰值时刻应为 (i+1)/fps");
        assert_eq!(per_second.len(), 20);
    }

    /// 已知周期的 onset → BPM 估计落在等价网格上
    #[test]
    fn bpm_from_periodic_onsets() {
        let onsets: Vec<f64> = (0..40).map(|k| 0.21 + k as f64 * 0.6857).collect();
        let bpm = estimate_bpm(&onsets).expect("应估出 BPM");
        // 0.6857s 周期 = 87.5 BPM（或其整倍/约分网格）
        assert!(
            (87.5 - bpm).abs() < 1.0 || (175.0 - bpm).abs() < 1.0 || (43.75 - bpm).abs() < 1.0,
            "bpm={bpm}"
        );
    }

    /// 无音轨 / 空输入不 panic，降级路径可用
    #[test]
    fn empty_pcm_is_safe() {
        assert!(onset_table(&[]).is_empty());
        let (p, o) = alignment(&[], &[1.0]);
        assert_eq!((p, o), (0.0, 0.0));
        assert_eq!(estimate_bpm(&[0.1, 0.2]), None);
    }
}
