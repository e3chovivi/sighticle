/**
 * 录制控制条 + 录后编辑器：独立小窗口（index.html#recbar）。
 * 阶段流：recording（计时）→ stopping（收尾）→ editor（预览+选段）→ exporting → done。
 *
 * 窗口几何/显隐一律通过 Rust 命令（layout_recbar / hide_recbar）完成：
 * `core:window:default` 只有只读权限，前端 JS 的 setSize/hide 会被权限系统拒绝且
 * 异常被吞掉，导致窗口不放大、按钮被裁出可视区域。
 * 每次新录制开始时后端发 "rec-started" 事件，据此重置状态（不依赖焦点事件）。
 */
import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { save } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import "./App.css";

type Phase = "recording" | "stopping" | "editor" | "exporting" | "done";

/// 字幕识别状态（导出完成后自动触发）
type SubState =
  | { phase: "running" }
  | { phase: "done"; path: string; embedded: boolean }
  | { phase: "error"; msg: string }
  | null;

/** 后端 "rec-started" 事件载荷：实际启用成功的音频轨道与告警 */
interface RecStartedPayload {
  mic: boolean;
  sys_audio: boolean;
  audio_warnings: string[];
}

/// 内联小图标：不引图标库（保持轻量）
const UI_ICONS = {
  stop: (
    <svg viewBox="0 0 24 24" fill="currentColor">
      <rect x="6" y="6" width="12" height="12" rx="2" />
    </svg>
  ),
  checkCircle: (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
      <circle cx="12" cy="12" r="9" />
      <path d="m8 12.5 2.8 2.8L16.5 9.5" />
    </svg>
  ),
};

export default function RecBar() {
  const [phase, setPhase] = createSignal<Phase>("recording");
  const [elapsed, setElapsed] = createSignal(0);
  const [srcPath, setSrcPath] = createSignal("");
  const [duration, setDuration] = createSignal(0);
  const [selStart, setSelStart] = createSignal(0);
  const [selEnd, setSelEnd] = createSignal(0);
  const [resultPath, setResultPath] = createSignal("");
  const [error, setError] = createSignal("");
  const [subState, setSubState] = createSignal<SubState>(null);
  // 字幕识别是否已启用（设置里填了凭证并打开开关）→ 编辑器里给出可感知的提示
  const [subEnabled, setSubEnabled] = createSignal(false);
  const [fallbackDur, setFallbackDur] = createSignal(0);
  // 本次录制实际启用的音频轨道（由 rec-started 事件告知）
  const [micOn, setMicOn] = createSignal(false);
  const [sysOn, setSysOn] = createSignal(false);
  const [audioWarn, setAudioWarn] = createSignal("");
  let timer: ReturnType<typeof setInterval> | null = null;
  let videoRef!: HTMLVideoElement;
  let trackRef!: HTMLDivElement;
  const unlistenFns: (() => void)[] = [];

  const fmtClock = (sec: number) => {
    const m = Math.floor(sec / 60);
    const s = Math.floor(sec % 60);
    return `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
  };
  const fmtSec = (sec: number) => `${sec.toFixed(1)}s`;
  const pct = (t: number) => (duration() > 0 ? `${(t / duration()) * 100}%` : "0%");

  function startTimer() {
    if (!timer) timer = setInterval(() => setElapsed((v) => v + 1), 1000);
  }
  function stopTimer() {
    if (timer) {
      clearInterval(timer);
      timer = null;
    }
  }

  /// 窗口布局与显隐交由 Rust 侧执行（见文件头说明）
  async function layout(stage: "rec" | "editor" | "done") {
    try {
      await invoke("layout_recbar", { stage });
    } catch (err) {
      console.error("layout_recbar failed:", err);
    }
  }
  async function hideWindow() {
    try {
      await invoke("hide_recbar");
    } catch (err) {
      console.error("hide_recbar failed:", err);
    }
  }

  /// 停止预览并释放媒体资源。
  ///
  /// 必须显式调用：窗口只是被 hide（不是销毁），而 Chromium 在元素被移出 DOM 后
  /// **不会自动停止播放**，于是会出现「界面已消失、声音还在后台响」的情况
  /// （若录像很长，就会一直响下去）。removeAttribute("src") + load() 才能真正
  /// 释放解码器与音频输出。
  function releasePreview() {
    const v = videoRef as HTMLVideoElement | undefined;
    if (!v) return;
    try {
      v.pause();
      v.removeAttribute("src");
      v.load();
    } catch {
      /* 元素可能已被卸载，忽略 */
    }
  }

  function resetToRecording() {
    releasePreview(); // 若上一次的编辑器还开着，先停掉预览再进入新录制
    stopTimer();
    setElapsed(0);
    setError("");
    setResultPath("");
    setSrcPath("");
    setSubState(null);
    setPhase("recording");
    startTimer();
    void layout("rec");
  }

  onMount(() => {
    startTimer();
    // 查询字幕开关状态（读取失败按未启用处理，只是不显示提示）
    void invoke<{ subtitle?: { enabled?: boolean; creds_ready?: boolean } }>("get_settings")
      .then((s) => setSubEnabled(!!(s.subtitle && s.subtitle.enabled && s.subtitle.creds_ready)))
      .catch(() => {});
    void listen<RecStartedPayload>("rec-started", (e) => {
      setMicOn(e.payload.mic);
      setSysOn(e.payload.sys_audio);
      setAudioWarn((e.payload.audio_warnings ?? []).join("；"));
      resetToRecording();
    })
      .then((fn) => unlistenFns.push(fn))
      .catch((err) => console.error("listen rec-started failed:", err));
    void layout("rec");
  });
  onCleanup(() => {
    for (const fn of unlistenFns) fn();
    stopTimer();
  });

  async function stop(discard: boolean) {
    if (phase() !== "recording") return;
    stopTimer();
    setPhase("stopping");
    try {
      const res = await Promise.race([
        invoke<{ path: string | null; seconds: number }>("stop_recording", { discard }),
        new Promise<never>((_, rej) => setTimeout(() => rej(new Error("停止超时")), 25000)),
      ]);
      if (discard || !res.path) {
        await hideWindow();
        setPhase("recording");
        return;
      }
      setSrcPath(res.path);
      setFallbackDur(res.seconds);
      setPhase("editor");
      await layout("editor");
    } catch (err) {
      console.error("stop_recording failed:", err);
      await hideWindow();
      setPhase("recording");
    }
  }

  /// 时间轴拖柄：拖动时实时更新选段并预览定位
  function beginDrag(which: "start" | "end") {
    return (e: PointerEvent) => {
      e.preventDefault();
      const rect = trackRef.getBoundingClientRect();
      const onMove = (ev: PointerEvent) => {
        const t = ((ev.clientX - rect.left) / rect.width) * duration();
        const clamped = Math.min(Math.max(t, 0), duration());
        if (which === "start") {
          const v = Math.max(0, Math.min(clamped, selEnd() - 0.1));
          setSelStart(v);
          if (videoRef) videoRef.currentTime = v;
        } else {
          const v = Math.min(duration(), Math.max(clamped, selStart() + 0.1));
          setSelEnd(v);
          if (videoRef) videoRef.currentTime = v;
        }
      };
      const onUp = () => {
        window.removeEventListener("pointermove", onMove);
        window.removeEventListener("pointerup", onUp);
      };
      window.addEventListener("pointermove", onMove);
      window.addEventListener("pointerup", onUp);
    };
  }

  /// 控制条整体可拖动：按下非交互区域（计时、音轨标签、空白）即拖走窗口，
  /// 按钮/输入不受影响。录制条常驻屏幕顶部会挡住浏览器标签页，必须能挪。
  function beginBarDrag(e: PointerEvent) {
    if (e.button !== 0) return;
    const t = e.target as HTMLElement | null;
    if (t?.closest("button, input, video, select, textarea")) return;
    void getCurrentWindow()
      .startDragging()
      .catch(() => {
        /* 无权限/非 Tauri 环境（浏览器预览）时忽略 */
      });
  }

  function stampName(ext: string) {
    return `录屏_${new Date().toISOString().slice(0, 19).replace(/[-:T]/g, "")}.${ext}`;
  }

  /// 导出完成后自动生成字幕（未配置时后端返回空串 → 静默跳过，不打扰用户）。
  /// 识别成功后字幕已烧录进画面：任何播放器点开即见，.srt 留给 AI 读取。
  async function runSubtitle(video: string) {
    setSubState({ phase: "running" });
    try {
      const out = await invoke<{ srt: string; embedded: boolean }>("generate_subtitle", { video });
      if (!out.srt) {
        setSubState(null); // 字幕未启用
        return;
      }
      setSubState({ phase: "done", path: out.srt, embedded: out.embedded });
    } catch (e) {
      setSubState({ phase: "error", msg: String(e) });
    }
  }

  async function exportAs(format: "mp4" | "gif") {
    if (phase() !== "editor") return;
    setError("");
    const ext = format === "gif" ? "gif" : "mp4";
    // 与截图共用「保存位置」约定：设了固定目录就直接存（不弹框），否则每次询问。
    let dest = "";
    let saveDir = "";
    try {
      const cfg = await invoke<{ save_dir: string }>("get_settings");
      saveDir = (cfg.save_dir ?? "").trim();
    } catch {
      /* 读不到设置就按「每次询问」处理 */
    }
    if (saveDir) {
      const dir = saveDir.replace(/[\\/]+$/, "");
      dest = `${dir}\\${stampName(ext)}`;
    } else {
      const picked = await save({
        defaultPath: stampName(ext),
        filters: [{ name: format === "gif" ? "GIF 动图" : "MP4 视频", extensions: [ext] }],
        title: "保存录屏",
      });
      if (!picked) return; // 用户取消，留在编辑器
      dest = picked;
    }
    setPhase("exporting");
    try {
      const path = await invoke<string>("export_recording", {
        src: srcPath(),
        start: selStart(),
        end: selEnd(),
        dest,
        format,
      });
      releasePreview(); // 预览已无用，避免导出完成后仍在后台出声
      setResultPath(path);
      setPhase("done");
      await layout("done");
      // 导出完成 → 自动识别字幕（未启用则后端静默返回）。
      // GIF 没有音轨，识别必然失败，跳过避免假报错。
      if (format === "mp4") void runSubtitle(path);
    } catch (err) {
      setError(`导出失败：${err}`);
      setPhase("editor");
    }
  }

  async function discardEditor() {
    releasePreview(); // 关键：先停预览，否则窗口隐藏后声音仍在后台播放
    try {
      await invoke("discard_recording", { src: srcPath() });
    } catch {
      /* 临时文件已不存在则忽略 */
    }
    await hideWindow();
  }

  async function finish() {
    releasePreview();
    try {
      await invoke("discard_recording", { src: srcPath() });
    } catch {
      /* 忽略 */
    }
    await hideWindow();
  }

  async function backToEditor() {
    setSubState(null); // 重新导出时字幕状态清零
    setPhase("editor");
    await layout("editor");
  }

  return (
    <div class="recbar-page">
      {/* ---------- 录制中 ---------- */}
      <Show when={phase() === "recording" || phase() === "stopping"}>
        <div class="rec-bar recbar-inline" onPointerDown={beginBarDrag}>
          <span class="rec-dot" />
          <span class="rec-time">{fmtClock(elapsed())}</span>
          <Show when={micOn()}>
            <span class="rec-track" title="正在录制麦克风">
              麦克风
            </span>
          </Show>
          <Show when={sysOn()}>
            <span class="rec-track" title="正在录制系统声音">
              系统声
            </span>
          </Show>
          <Show when={audioWarn()}>
            <span class="rec-track warn" title={audioWarn()}>
              音频异常
            </span>
          </Show>
          <button class="rec-btn stop" disabled={phase() === "stopping"} onClick={() => void stop(false)}>
            {phase() === "stopping" ? (
              "处理中…"
            ) : (
              <>
                {UI_ICONS.stop}
                停止
              </>
            )}
          </button>
          <button class="rec-btn discard" disabled={phase() === "stopping"} onClick={() => void stop(true)}>
            放弃
          </button>
        </div>
      </Show>

      {/* ---------- 录后编辑器 ---------- */}
      <Show when={phase() === "editor" || phase() === "exporting"}>
        <div class="recbar-editor">
          <video
            ref={videoRef}
            class="editor-video"
            src={srcPath() ? convertFileSrc(srcPath()) : undefined}
            controls
            onLoadedMetadata={(e) => {
              const d = e.currentTarget.duration;
              const dur = Number.isFinite(d) && d > 0 ? d : fallbackDur();
              setDuration(dur);
              setSelStart(0);
              setSelEnd(dur);
            }}
          />
          <div
            class="tl"
            ref={trackRef}
            onClick={(e) => {
              if (!videoRef || duration() <= 0) return;
              const rect = trackRef.getBoundingClientRect();
              const t = ((e.clientX - rect.left) / rect.width) * duration();
              videoRef.currentTime = Math.min(Math.max(t, 0), duration());
            }}
          >
            <div class="tl-sel" style={{ left: pct(selStart()), width: pct(selEnd() - selStart()) }} />
            <div class="tl-handle" style={{ left: pct(selStart()) }} onPointerDown={beginDrag("start")} />
            <div class="tl-handle" style={{ left: pct(selEnd()) }} onPointerDown={beginDrag("end")} />
          </div>
          <div class="tl-readout">
            {fmtSec(selStart())} – {fmtSec(selEnd())} · 保留 {fmtSec(selEnd() - selStart())} / 全长{" "}
            {fmtSec(duration())}
          </div>
          <Show when={subEnabled()}>
            <div class="editor-sub-hint">已启用字幕识别 · 保存 MP4 后自动识别并烧录进画面</div>
          </Show>
          <div class="recbar-actions">
            <button class="rec-btn discard" disabled={phase() === "exporting"} onClick={() => void discardEditor()}>
              放弃
            </button>
            <button class="rec-btn open" disabled={phase() === "exporting"} onClick={() => void exportAs("gif")}>
              存为 GIF
            </button>
            <button class="rec-btn finish" disabled={phase() === "exporting"} onClick={() => void exportAs("mp4")}>
              保存 MP4
            </button>
          </div>
          <Show when={phase() === "exporting"}>
            <div class="tl-readout">正在导出，请稍候…</div>
          </Show>
          <Show when={error()}>
            <div class="tl-error">{error()}</div>
          </Show>
        </div>
      </Show>

      {/* ---------- 完成 ---------- */}
      <Show when={phase() === "done"}>
        <div class="recbar-result">
          <div class="result-title">
            {UI_ICONS.checkCircle}
            已保存
          </div>
          <div class="result-path">{resultPath()}</div>

          {/* 字幕识别状态：导出后自动触发；未配置则整行不出现 */}
          <Show when={subState()}>
            <div class="sub-status" classList={{ bad: subState()!.phase === "error" }}>
              <span>
                {subState()!.phase === "running"
                  ? "正在识别字幕…（通常几秒~半分钟）"
                  : subState()!.phase === "error"
                    ? `字幕识别失败：${(subState() as { phase: "error"; msg: string }).msg}`
                    : (subState() as { phase: "done"; path: string; embedded: boolean }).embedded
                      ? "字幕已生成并烧录进视频（任何播放器可见）"
                      : "字幕已生成（.srt，烧录进画面失败）"}
              </span>
              <Show when={subState()!.phase === "done"}>
                <button
                  class="sub-open"
                  onClick={() =>
                    void revealItemInDir((subState() as { phase: "done"; path: string }).path)
                  }
                >
                  打开文件夹
                </button>
              </Show>
            </div>
          </Show>

          <div class="recbar-actions">
            <button class="rec-btn open" onClick={() => void revealItemInDir(resultPath())}>
              打开文件夹
            </button>
            <button class="rec-btn discard" onClick={() => void backToEditor()}>
              再导出
            </button>
            <button class="rec-btn finish" onClick={() => void finish()}>
              完成
            </button>
          </div>
        </div>
      </Show>
    </div>
  );
}
