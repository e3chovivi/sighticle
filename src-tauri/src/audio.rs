//! 音频采集：麦克风 + 系统声音（WASAPI loopback）
//!
//! 全部由 cpal 原生实现，**不需要安装虚拟声卡或任何第三方驱动**：
//! - 麦克风：在默认**输入**设备上建输入流；
//! - 系统声音：在默认**输出**设备上建**输入**流 —— cpal/WASAPI 会自动置
//!   `AUDCLNT_STREAMFLAGS_LOOPBACK`，从而捕获系统正在播放的混音。
//!   注意输出设备没有 input config，必须用 `default_output_config()` 配置。
//!
//! 两路各自统一转成 16-bit PCM 写入 WAV；**重采样与混音全部交给 FFmpeg**
//! 在停止阶段完成，Rust 侧只做最薄的采集与落盘，降低出错面。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

/// 停止时等待写盘线程收敛的上限
const FINISH_TIMEOUT: Duration = Duration::from_secs(3);

/// 单路音频采集（一路麦克风或一路系统声音）
struct Track {
    /// 持有 cpal 流以维持采集；置 None 并 drop 即停止采集
    stream: Option<cpal::Stream>,
    writer: JoinHandle<()>,
    /// 通知写盘线程收尾（不依赖回调释放 sender，避免停止被卡住）
    stop: Arc<AtomicBool>,
    path: PathBuf,
}

impl Track {
    /// 停止采集、等 WAV 落盘完成。全程限时，绝不拖死调用方（录制停止流程）。
    fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.stream.take());
        let deadline = Instant::now() + FINISH_TIMEOUT;
        while !self.writer.is_finished() {
            if Instant::now() > deadline {
                // 极端情况：写盘线程未及时收敛，放弃等待（WAV 可能不完整，但不阻塞用户）
                std::mem::forget(self.writer);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.writer.join();
    }
}

/// 一次录制的音频会话（可含麦克风与系统声音两路）
struct AudioSession {
    tracks: Vec<Track>,
}

impl AudioSession {
    /// 停止采集并返回已落盘的音频文件路径（顺序：麦克风、系统声音）
    fn finish(self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for t in self.tracks {
            let p = t.path.clone();
            t.finish();
            out.push(p);
        }
        out
    }

    #[cfg(test)]
    fn track_count(&self) -> usize {
        self.tracks.len()
    }
}

/// 可回传给前端的音频启动情况
#[derive(Clone, Debug, serde::Serialize)]
pub struct AudioReport {
    /// 麦克风是否成功启用
    pub mic: bool,
    /// 系统声音是否成功启用
    pub sys: bool,
    /// 单路失败时的告警（不阻断录制，界面可提示）
    pub warnings: Vec<String>,
}

impl AudioReport {
    fn none() -> Self {
        Self {
            mic: false,
            sys: false,
            warnings: Vec::new(),
        }
    }
}

// ================= 专用采集线程（COM 隔离，必须） =================
//
// 为什么音频必须跑在自己的线程上：cpal/WASAPI 与 xcap/WGC 都要使用 COM，而 COM 的
// 线程套间（apartment）状态是**按线程**保存的。两者混用同一条线程会互相污染——
// 实测表现为「第一次录屏一切正常，第二次启动屏幕采集直接报参数错误（E_INVALIDARG）」。
// 把音频整体隔离到独立线程后，两条线程各自持有干净的 COM 状态，互不干扰。

enum AudioCmd {
    Start {
        dir: PathBuf,
        mic: bool,
        sys: bool,
        reply: SyncSender<AudioReport>,
    },
    Stop {
        reply: SyncSender<Vec<PathBuf>>,
    },
}

static AUDIO_TX: OnceLock<SyncSender<AudioCmd>> = OnceLock::new();

/// 音频采集线程的 id（用于结构性回归测试，确保 COM 隔离不被破坏）
static AUDIO_THREAD_ID: OnceLock<std::thread::ThreadId> = OnceLock::new();

/// 测试辅助：音频采集线程的 id（尚未启动时返回 None）
#[cfg(test)]
pub fn capture_thread_id() -> Option<std::thread::ThreadId> {
    AUDIO_THREAD_ID.get().copied()
}

fn audio_tx() -> &'static SyncSender<AudioCmd> {
    AUDIO_TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<AudioCmd>(4);
        std::thread::Builder::new()
            .name("lc-audio".into())
            .spawn(move || {
                let _ = AUDIO_THREAD_ID.set(std::thread::current().id());
                let mut session: Option<AudioSession> = None;
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        AudioCmd::Start {
                            dir,
                            mic,
                            sys,
                            reply,
                        } => {
                            // 防御：若上一次会话没被正常收尾，先收干净再开新会话
                            if let Some(prev) = session.take() {
                                let _ = prev.finish();
                            }
                            let outcome = start(&dir, mic, sys);
                            if !outcome.session.tracks.is_empty() {
                                session = Some(outcome.session);
                            }
                            let _ = reply.send(outcome.report);
                        }
                        AudioCmd::Stop { reply } => {
                            let files = match session.take() {
                                Some(s) => s.finish(),
                                None => Vec::new(),
                            };
                            let _ = reply.send(files);
                        }
                    }
                }
            })
            .expect("无法启动音频采集线程");
        tx
    })
}

/// 启动音频采集（在专用线程上执行）。均不致命：单路失败只记告警。
pub fn start_capture(dir: &Path, mic: bool, sys: bool) -> AudioReport {
    if !mic && !sys {
        return AudioReport::none();
    }
    let (tx, rx) = sync_channel(1);
    if audio_tx()
        .send(AudioCmd::Start {
            dir: dir.to_path_buf(),
            mic,
            sys,
            reply: tx,
        })
        .is_err()
    {
        return AudioReport {
            mic: false,
            sys: false,
            warnings: vec!["音频线程未运行".into()],
        };
    }
    // 设备打开偶有延迟，给足 10 秒；超时则视为未启用（不阻断录像）
    rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| AudioReport {
        mic: false,
        sys: false,
        warnings: vec!["音频启动超时".into()],
    })
}

/// 停止音频采集并返回落盘的音频文件路径（无会话时返回空）。
pub fn stop_capture() -> Vec<PathBuf> {
    let (tx, rx) = sync_channel(1);
    if audio_tx().send(AudioCmd::Stop { reply: tx }).is_err() {
        return Vec::new();
    }
    // finish 内部已限时（每路 3 秒），这里再加一层总超时兜底
    rx.recv_timeout(Duration::from_secs(12)).unwrap_or_default()
}

// ================= 采集实现（仅由音频线程调用） =================

struct StartOutcome {
    session: AudioSession,
    report: AudioReport,
}

/// 启动音频采集。want_mic / want_sys 指定需要采集的轨道；单路失败不影响另一路。
fn start(dir: &Path, want_mic: bool, want_sys: bool) -> StartOutcome {
    let mut tracks = Vec::new();
    let mut warnings = Vec::new();
    let mut mic_ok = false;
    let mut sys_ok = false;
    let host = cpal::default_host();

    if want_mic {
        match start_mic(&host, dir) {
            Ok(t) => {
                tracks.push(t);
                mic_ok = true;
            }
            Err(e) => warnings.push(format!("麦克风：{e}")),
        }
    }
    if want_sys {
        match start_loopback(&host, dir) {
            Ok(t) => {
                tracks.push(t);
                sys_ok = true;
            }
            Err(e) => warnings.push(format!("系统声音：{e}")),
        }
    }
    StartOutcome {
        session: AudioSession { tracks },
        report: AudioReport {
            mic: mic_ok,
            sys: sys_ok,
            warnings,
        },
    }
}

/// 采集到的 WAV 是否含有效音频数据（大于 RIFF 头 44 字节）。
/// 作为混流前的安全网：文件缺失或写盘失败时不送去 FFmpeg。
pub fn has_audio_data(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 44).unwrap_or(false)
}

/// 按墙钟把静音补齐到"此刻"，保证音频时间轴与真实录制时长一致。
///
/// 必要性：WASAPI loopback 在系统**静音或空闲时根本不投递数据**。若只是顺序写盘，
/// 那么"第 5 秒才开始有声音"会被写成"从 0 秒开始有声音"，导致音画不同步。
/// 先补齐静音再写入真实采样，真实声音就会落在它实际发生的时刻。
fn pad_silence<W: std::io::Write + std::io::Seek>(
    w: &mut hound::WavWriter<W>,
    written_frames: &mut u64,
    started: Instant,
    rate: u32,
    channels: u16,
) -> bool {
    let target = (started.elapsed().as_secs_f64() * rate as f64) as u64;
    while *written_frames < target {
        for _ in 0..channels {
            if w.write_sample(0i16).is_err() {
                return false;
            }
        }
        *written_frames += 1;
    }
    true
}

fn start_mic(host: &cpal::Host, dir: &Path) -> Result<Track, String> {
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            crate::lclog!("麦克风采集：未找到默认输入设备");
            return Err("未找到麦克风设备".into());
        }
    };
    let config = device.default_input_config().map_err(|e| {
        crate::lclog!("麦克风采集：读取设备配置失败 {e:?}");
        format!("读取麦克风配置失败：{e}")
    })?;
    start_track(&device, &config, &dir.join("mic.wav"))
}

/// 系统声音：在输出设备上建输入流，WASAPI 会自动启用 loopback
fn start_loopback(host: &cpal::Host, dir: &Path) -> Result<Track, String> {
    let device = match host.default_output_device() {
        Some(d) => d,
        None => {
            crate::lclog!("系统声音采集：未找到默认输出设备");
            return Err("未找到扬声器设备".into());
        }
    };
    let config = device.default_output_config().map_err(|e| {
        crate::lclog!("系统声音采集：读取设备配置失败 {e:?}");
        format!("读取扬声器配置失败：{e}")
    })?;
    start_track(&device, &config, &dir.join("sys.wav"))
}

fn start_track(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    path: &Path,
) -> Result<Track, String> {
    let channels = config.channels();
    let rate = config.sample_rate();
    let sample_format = config.sample_format();
    let stream_config = config.config();

    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // cpal 0.18 用 description() 取代了旧的 name()
    let device_label = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "未知设备".into());
    crate::lclog!(
        "音频轨 {label}：设备={device_label} 声道={channels} 采样率={rate}Hz 格式={sample_format:?}"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = stop.clone();
    let (tx, rx) = channel::<Vec<i16>>();
    let writer_path = path.to_path_buf();
    let writer = std::thread::spawn(move || {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = match hound::WavWriter::create(&writer_path, spec) {
            Ok(w) => w,
            Err(_) => return,
        };
        let ch = channels as u64;
        let started = Instant::now();
        let mut written_frames: u64 = 0;
        // 收到停止标志或通道关闭即收尾，保证 WAV 头部被正确 finalize
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(buf) => {
                    // 先把静音补齐到"此刻"，再写入这批真实采样（落在其实际发生的时刻）
                    if !pad_silence(&mut w, &mut written_frames, started, rate, channels) {
                        return;
                    }
                    let frames = buf.len() as u64 / ch;
                    for s in buf {
                        if w.write_sample(s).is_err() {
                            return;
                        }
                    }
                    written_frames += frames;
                }
                Err(RecvTimeoutError::Timeout) => {
                    // 空闲期（如系统静音）没有数据：继续按墙钟补静音，维持时间轴
                    if !pad_silence(&mut w, &mut written_frames, started, rate, channels) {
                        return;
                    }
                    if writer_stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = w.finalize();
    });

    let stream = {
        // 只保留闭包里的那一份 sender：本块结束后 txc 已随 build 移入回调
        let txc = tx.clone();
        let built = match sample_format {
            SampleFormat::F32 => build::<f32>(device, stream_config.clone(), txc),
            SampleFormat::I16 => build::<i16>(device, stream_config.clone(), txc),
            SampleFormat::I32 => build::<i32>(device, stream_config.clone(), txc),
            SampleFormat::U16 => build::<u16>(device, stream_config.clone(), txc),
            SampleFormat::U32 => build::<u32>(device, stream_config.clone(), txc),
            other => Err(format!("不支持的采样格式 {other:?}")),
        };
        match built {
            Ok(s) => s,
            Err(e) => {
                crate::lclog!("音频轨 {label} 建流失败：{e}");
                return Err(e);
            }
        }
    };
    // 释放外部持有的 sender（回调里那份保留）
    drop(tx);

    if let Err(e) = stream.play() {
        crate::lclog!("音频轨 {label} 启动失败：{e:?}");
        return Err(format!("启动音频流失败：{e}"));
    }
    crate::lclog!("音频轨 {label} 已开始采集");
    Ok(Track {
        stream: Some(stream),
        writer,
        stop,
        path: path.to_path_buf(),
    })
}

/// 构建输入流：回调内把设备原生采样格式统一转成 i16 后交给写盘线程。
/// 回调保持简单（不做磁盘 IO），避免影响实时性。
fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: Sender<Vec<i16>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + Send + 'static,
    i16: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let buf: Vec<i16> = data.iter().map(|s| (*s).to_sample::<i16>()).collect();
                let _ = tx.send(buf);
            },
            |err| eprintln!("[light-capture] 音频流错误: {err}"),
            None,
        )
        .map_err(|e| format!("打开音频设备失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// 结构性回归：音频采集必须发生在**独立线程**上，而不是调用线程。
    ///
    /// 这是「第二次录屏报参数错误」修复的核心约束：cpal/WASAPI 与 xcap/WGC 共用 COM，
    /// 而 COM 套间状态按线程保存。若把音频采集挪回屏幕采集所在线程（或直接跑在调用
    /// 线程上），该线程的 COM 状态会被污染，第二次启动屏幕采集即报 E_INVALIDARG。
    /// 一旦有人这么「优化」，本测试会立刻失败。
    #[test]
    fn audio_capture_is_isolated_on_its_own_thread() {
        let dir = fresh_dir("lc-audio-isolation");
        let caller = std::thread::current().id();
        let _ = start_capture(&dir, false, true);
        let _ = stop_capture();
        let worker = capture_thread_id().expect("音频采集线程应已启动");
        assert_ne!(
            worker, caller,
            "音频采集必须运行在独立线程上（COM 套间隔离），不能与调用方同线程"
        );
    }

    /// 未请求任何音频时：空会话、无告警
    #[test]
    fn no_audio_requested_yields_empty_session() {
        let dir = fresh_dir("lc-audio-none");
        let o = start(&dir, false, false);
        assert_eq!(o.session.track_count(), 0);
        assert!(o.report.warnings.is_empty());
        assert!(!o.report.mic && !o.report.sys);
    }

    /// 请求采集但设备缺失时：不得 panic，且**不允许静默失败**——未启用必须给告警
    #[test]
    fn start_degrades_gracefully_without_devices() {
        let dir = fresh_dir("lc-audio-degrade");
        let o = start(&dir, true, true);
        assert!(
            o.report.mic || o.report.warnings.iter().any(|w| w.starts_with("麦克风")),
            "麦克风未启用时必须给出告警，不能静默失败"
        );
        assert!(
            o.report.sys || o.report.warnings.iter().any(|w| w.starts_with("系统声音")),
            "系统声音未启用时必须给出告警，不能静默失败"
        );
    }

    /// 静音补齐：空闲期（系统静音时 loopback 不投递数据）也必须按墙钟推进时间轴，
    /// 否则"第 5 秒才有声音"会被写成"从 0 秒开始"，造成音画不同步。
    #[test]
    fn pad_silence_advances_timeline() {
        let dir = fresh_dir("lc-audio-pad");
        let path = dir.join("pad.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        let mut written = 0u64;
        // 模拟：已过去 100ms 却一个采样都没收到
        let started = Instant::now() - Duration::from_millis(100);
        assert!(pad_silence(&mut w, &mut written, started, 8_000, 1));
        w.finalize().unwrap();
        // 8kHz × 0.1s ≈ 800 帧静音
        assert!(written >= 700, "应补齐约 800 帧静音，实际 {written}");
        assert!(has_audio_data(&path), "补齐后应被视为有效音频轨");
    }

    /// 端到端（走专用线程 API）：启动 → 采集 0.7 秒 → 停止，验证 WAV 落盘且格式合法。
    /// 无可用设备时跳过（本机开发机通常有设备，属有效验证）。
    #[test]
    fn capture_over_dedicated_thread_writes_valid_wav() {
        let dir = fresh_dir("lc-audio-thread");
        let report = start_capture(&dir, true, true);
        let files = stop_capture();
        if files.is_empty() {
            eprintln!("跳过 WAV 校验：本机无可用音频设备，告警={:?}", report.warnings);
            return;
        }
        // 再跑一轮完整采集，确认专用线程可反复启停（COM 隔离的回归验证）
        let report2 = start_capture(&dir, true, true);
        std::thread::sleep(Duration::from_millis(700));
        let files2 = stop_capture();
        assert_eq!(
            files2.len(),
            files.len(),
            "第二次采集的轨道数应与第一次一致（report={:?}）",
            report2
        );
        for f in &files2 {
            let bytes = std::fs::read(f).unwrap_or_else(|e| panic!("{} 未生成：{e}", f.display()));
            assert!(bytes.len() > 44, "{} 仅 {} 字节，疑似空音频", f.display(), bytes.len());
            assert_eq!(&bytes[0..4], b"RIFF", "{} 缺少 RIFF 头", f.display());
            assert_eq!(&bytes[8..12], b"WAVE", "{} 缺少 WAVE 标识", f.display());
        }
    }
}
