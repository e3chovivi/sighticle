/**
 * 设置窗口（index.html#settings，由托盘「设置…」或覆盖层里的「设置」按钮按需打开）。
 *
 * 包含三项：
 *  1. 全局快捷键 —— 点击后直接按键位录入，保存时向系统真实注册以检测冲突；
 *     若被其它程序占用，会明确报错并自动回退到旧键位（保证不会失去唤起能力）。
 *  2. 音频增益 —— 麦克风/系统声音分开调节（离得远导致声音小可调大）。
 *  3. 开机自启。
 */
import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import "./App.css";

interface SubtitleView {
  enabled: boolean;
  /** "qwen" | "doubao" */
  provider: string;
  qwen_api_key: string;
  doubao_app_key: string;
  doubao_access_token: string;
  /** 当前服务商凭证是否齐全（后端 sanitize 后的判定） */
  creds_ready: boolean;
}

interface FfmpegView {
  found: boolean;
  program: string;
  /** env | exe_dir | path | well_known */
  origin: string;
  version: string;
  /** 缺失时的安装建议 */
  hint: string;
}

interface SettingsView {
  hotkey: string;
  hotkey_display: string;
  mic_gain: number;
  sys_gain: number;
  autostart: boolean;
  cursor_overlay: boolean;
  cursor_glow: boolean;
  save_dir: string;
  log_path: string;
  ffmpeg: FfmpegView;
  mic_gain_range: [number, number];
  sys_gain_range: [number, number];
  kill_hotkey_display: string;
  subtitle: SubtitleView;
}

const MODIFIER_KEYS = ["Control", "Shift", "Alt", "Meta"];

/// FFmpeg 来源的中文说明
function originLabel(origin: string): string {
  switch (origin) {
    case "env":
      return "环境变量 SIGHTICLE_FFMPEG";
    case "exe_dir":
      return "程序同目录";
    case "path":
      return "系统 PATH";
    case "well_known":
      return "常见安装位置";
    default:
      return origin || "未知";
  }
}

export default function Settings() {
  const [view, setView] = createSignal<SettingsView | null>(null);
  const [recording, setRecording] = createSignal(false);
  const [captured, setCaptured] = createSignal("");
  const [msg, setMsg] = createSignal("");
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  /// 字幕「测试连通」结果：idle 不显示
  const [testState, setTestState] = createSignal<{
    kind: "idle" | "running" | "ok" | "err";
    msg: string;
  }>({ kind: "idle", msg: "" });

  function flash(ok: string | null, bad: string | null) {
    setMsg(ok ?? "");
    setErr(bad ?? "");
  }

  async function load() {
    try {
      setView(await invoke<SettingsView>("get_settings"));
    } catch (e) {
      flash(null, `读取设置失败：${e}`);
    }
  }

  onMount(() => {
    void load();
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
  });
  onCleanup(() => {
    window.removeEventListener("keydown", onKeyDown);
    window.removeEventListener("keyup", onKeyUp);
  });

  /// 录入中：组合出 "Ctrl+Shift+Q" 形式（至少需要一个修饰键，避免抢占普通按键）
  function onKeyDown(e: KeyboardEvent) {
    if (!recording()) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      setRecording(false);
      setCaptured("");
      flash("已取消录入", null);
      return;
    }
    if (MODIFIER_KEYS.includes(e.key)) {
      // 只按下修饰键：先给个预览，等主键
      setCaptured(modsOf(e).join("+"));
      return;
    }
    const mods = modsOf(e);
    if (mods.length === 0) {
      setCaptured("");
      flash(null, "请至少配合一个修饰键（Ctrl / Shift / Alt / Win）");
      return;
    }
    setCaptured([...mods, mainKey(e)].join("+"));
  }

  /// 松开主键即提交，交给后端做真正的系统级冲突检测
  async function onKeyUp(e: KeyboardEvent) {
    if (!recording()) return;
    e.preventDefault();
    if (MODIFIER_KEYS.includes(e.key)) return;
    const combo = captured();
    if (!combo) return;
    setRecording(false);
    setCaptured("");
    await applyHotkey(combo);
  }

  function modsOf(e: KeyboardEvent): string[] {
    const mods: string[] = [];
    if (e.ctrlKey) mods.push("Ctrl");
    if (e.shiftKey) mods.push("Shift");
    if (e.altKey) mods.push("Alt");
    if (e.metaKey) mods.push("Win");
    return mods;
  }

  function mainKey(e: KeyboardEvent): string {
    const k = e.key;
    if (k === " ") return "Space";
    return k.length === 1 ? k.toUpperCase() : k;
  }

  async function applyHotkey(combo: string) {
    setBusy(true);
    try {
      const next = await invoke<SettingsView>("set_hotkey", { hotkey: combo });
      setView(next);
      flash(`已切换为 ${next.hotkey_display}`, null);
    } catch (e) {
      flash(null, String(e));
    } finally {
      setBusy(false);
    }
  }

  async function applyGains(patch: { mic_gain?: number; sys_gain?: number }) {
    const cur = view();
    if (!cur) return;
    const mic = patch.mic_gain ?? cur.mic_gain;
    const sys = patch.sys_gain ?? cur.sys_gain;
    try {
      const next = await invoke<SettingsView>("set_audio_gains", {
        micGain: mic,
        sysGain: sys,
      });
      setView(next);
      flash("音频增益已保存", null);
    } catch (e) {
      flash(null, String(e));
    }
  }

  async function toggleCursorOverlay(enabled: boolean) {
    const v0 = view();
    if (!v0) return;
    try {
      setView(
        await invoke<SettingsView>("set_cursor_overlay", {
          cursor: enabled,
          glow: enabled ? v0.cursor_glow : false,
        }),
      );
      flash(enabled ? "录屏时将显示鼠标指针" : "录屏时不再显示鼠标指针", null);
    } catch (e) {
      flash(null, String(e));
    }
  }

  async function toggleCursorGlow(enabled: boolean) {
    const v0 = view();
    if (!v0) return;
    try {
      setView(
        await invoke<SettingsView>("set_cursor_overlay", {
          cursor: v0.cursor_overlay,
          glow: enabled,
        }),
      );
      flash(enabled ? "已开启指针光晕" : "已关闭指针光晕", null);
    } catch (e) {
      flash(null, String(e));
    }
  }

  async function toggleAutostart(enabled: boolean) {
    try {
      setView(await invoke<SettingsView>("set_autostart", { enabled }));
      flash(enabled ? "已开启开机自启" : "已关闭开机自启", null);
    } catch (e) {
      flash(null, String(e));
    }
  }

  /// 选一个固定文件夹保存截图；取消选择则不动
  async function pickSaveDir() {
    const dir = await open({ directory: true, title: "选择截图保存文件夹" });
    if (typeof dir !== "string") return;
    try {
      setView(await invoke<SettingsView>("set_save_dir", { dir }));
      flash(`截图将保存到 ${dir}`, null);
    } catch (e) {
      flash(null, String(e));
    }
  }

  /// 恢复「每次询问保存位置」
  async function clearSaveDir() {
    try {
      setView(await invoke<SettingsView>("set_save_dir", { dir: "" }));
      flash("已恢复每次询问保存位置", null);
    } catch (e) {
      flash(null, String(e));
    }
  }

  /// 凭证是否齐全（与后端 sanitize 同逻辑，供编辑过程中即时判定开关可用性）
  const credsReady = (s: SubtitleView) =>
    s.provider === "doubao"
      ? s.doubao_app_key.trim() !== ""
      : s.qwen_api_key.trim() !== "";

  /// 更新字幕配置的某个字段（纯本地，点「保存配置」才落盘）
  function patchSubtitle(patch: Partial<SubtitleView>) {
    const v = view();
    if (!v) return;
    const sub = { ...v.subtitle, ...patch };
    const wasReady = credsReady(v.subtitle);
    const ready = credsReady(sub);
    if (patch.qwen_api_key !== undefined || patch.doubao_app_key !== undefined) {
      if (!ready) sub.enabled = false;
      // 凭证补齐的瞬间自动启用（"填了就默认开启"），用户仍可手动关掉
      else if (!wasReady && ready) sub.enabled = true;
    }
    setView({ ...v, subtitle: sub });
    setTestState({ kind: "idle", msg: "" }); // 凭证/服务商一变，旧的测试结果作废
  }

  /// 保存字幕配置到后端（sanitize 会再兜一次底）
  async function saveSubtitle() {
    const sub = view()?.subtitle;
    if (!sub) return;
    try {
      setView(
        await invoke<SettingsView>("set_subtitle_config", {
          enabled: sub.enabled && credsReady(sub),
          provider: sub.provider,
          qwenApiKey: sub.qwen_api_key,
          doubaoAppKey: sub.doubao_app_key,
          doubaoAccessToken: sub.doubao_access_token,
        })
      );
      flash("字幕配置已保存", null);
    } catch (e) {
      flash(null, String(e));
    }
  }

  /// 测试连通性：先保存表单（所测即所得），再请后端用凭证发一次轻量请求
  async function testConnection() {
    const sub = view()?.subtitle;
    if (!sub || !credsReady(sub)) return;
    setTestState({ kind: "running", msg: "正在连接服务商…" });
    try {
      // 先落盘：后端测试读取的是已保存配置，避免"测的是没保存的 Key"
      setView(
        await invoke<SettingsView>("set_subtitle_config", {
          enabled: sub.enabled,
          provider: sub.provider,
          qwenApiKey: sub.qwen_api_key,
          doubaoAppKey: sub.doubao_app_key,
          doubaoAccessToken: sub.doubao_access_token,
        })
      );
      const msg = await invoke<string>("test_subtitle_connection");
      setTestState({ kind: "ok", msg });
    } catch (e) {
      setTestState({ kind: "err", msg: String(e) });
    }
  }

  return (
    <div class="settings-page">
      <div class="settings-head">
        <div class="settings-title">sighticle 设置</div>
        <button class="settings-close" onClick={() => void getCurrentWindow().close()}>
          关闭
        </button>
      </div>

      <Show when={view()} fallback={<div class="settings-loading">读取中…</div>}>
        {(v) => (
          <>
            {/* ---------- 全局快捷键 ---------- */}
            <section class="settings-card">
              <div class="settings-row">
                <div>
                  <div class="settings-label">唤起快捷键</div>
                  <div class="settings-hint">
                    按下后选择「截图 / 录屏」。保存时会向系统真实注册，若已被其它程序占用会明确提示。
                  </div>
                </div>
              </div>
              <div class="settings-row settings-row-inline">
                <div class="hotkey-box" classList={{ recording: recording() }}>
                  {recording()
                    ? captured() || "请按下组合键…（Esc 取消）"
                    : v().hotkey_display}
                </div>
                <button
                  class="settings-btn primary"
                  disabled={busy()}
                  onClick={() => {
                    setRecording(true);
                    setCaptured("");
                    flash("请按下新的组合键", null);
                  }}
                >
                  {recording() ? "录入中…" : "修改"}
                </button>
              </div>
              <div class="settings-hint">
                强制退出键位固定为 {v().kill_hotkey_display}（防止界面异常时无法脱身）
              </div>
            </section>

            {/* ---------- 音频增益 ---------- */}
            <section class="settings-card">
              <div class="settings-label">音频增益</div>
              <div class="settings-hint">录屏时生效。麦克风声音偏小可以调大（过大可能失真）。</div>
              <div class="settings-row settings-row-inline">
                <span class="gain-name">麦克风</span>
                <input
                  type="range"
                  min={v().mic_gain_range[0]}
                  max={v().mic_gain_range[1]}
                  step="0.1"
                  value={v().mic_gain}
                  onInput={(e) => {
                    const g = Number(e.currentTarget.value);
                    setView({ ...v(), mic_gain: g });
                  }}
                  onChange={(e) => void applyGains({ mic_gain: Number(e.currentTarget.value) })}
                />
                <span class="gain-value">{v().mic_gain.toFixed(1)}×</span>
              </div>
              <div class="settings-row settings-row-inline">
                <span class="gain-name">系统声音</span>
                <input
                  type="range"
                  min={v().sys_gain_range[0]}
                  max={v().sys_gain_range[1]}
                  step="0.1"
                  value={v().sys_gain}
                  onInput={(e) => {
                    const g = Number(e.currentTarget.value);
                    setView({ ...v(), sys_gain: g });
                  }}
                  onChange={(e) => void applyGains({ sys_gain: Number(e.currentTarget.value) })}
                />
                <span class="gain-value">{v().sys_gain.toFixed(1)}×</span>
              </div>
            </section>

            {/* ---------- 截图/录屏保存位置 ---------- */}
            <section class="settings-card">
              <div class="settings-label">截图/录屏保存位置</div>
              <div class="settings-hint">
                截图「保存」和录屏「导出」时：每次询问保存位置，或固定保存到一个文件夹（文件名自动按时间生成）。
              </div>
              <div class="settings-row settings-row-inline">
                <div class="hotkey-box">{v().save_dir || "每次询问保存位置"}</div>
                <button class="settings-btn primary" onClick={() => void pickSaveDir()}>
                  选择文件夹
                </button>
                <button class="settings-btn" disabled={!v().save_dir} onClick={() => void clearSaveDir()}>
                  恢复询问
                </button>
              </div>
            </section>

            {/* ---------- 字幕识别 ---------- */}
            <section class="settings-card">
              <div class="settings-row settings-row-inline">
                <div>
                  <div class="settings-label">字幕识别</div>
                  <div class="settings-hint">
                    导出录屏后自动识别语音，生成同名 .srt 字幕文件。填入凭证即启用。
                  </div>
                </div>
                <button
                  class="switch"
                  classList={{
                    on: v().subtitle.enabled && credsReady(v().subtitle),
                  }}
                  disabled={!credsReady(v().subtitle)}
                  role="switch"
                  aria-checked={v().subtitle.enabled && credsReady(v().subtitle)}
                  aria-label="字幕识别开关"
                  title={
                    credsReady(v().subtitle)
                      ? "导出后自动生成字幕"
                      : "请先填入所选服务商的凭证并保存"
                  }
                  onClick={() =>
                    patchSubtitle({
                      enabled: !(v().subtitle.enabled && credsReady(v().subtitle)),
                    })
                  }
                >
                  <span class="switch-knob" />
                </button>
              </div>

              {/* 凭证已就绪但开关没开：导出不会生成字幕，必须显眼地拦一道 */}
              <Show when={credsReady(v().subtitle) && !v().subtitle.enabled}>
                <div class="sub-warning">
                  已填入凭证，但开关处于关闭状态——导出后不会生成字幕。请打开上方开关并保存。
                </div>
              </Show>

              {/* 服务商切换 */}
              <div class="settings-row">
                <div class="settings-hint" style={{ "margin-bottom": "6px" }}>
                  服务商
                </div>
                <div class="provider-tabs">
                  <button
                    class="provider-tab"
                    classList={{ on: v().subtitle.provider === "qwen" }}
                    onClick={() => patchSubtitle({ provider: "qwen" })}
                  >
                    Qwen · 阿里
                  </button>
                  <button
                    class="provider-tab"
                    classList={{ on: v().subtitle.provider === "doubao" }}
                    onClick={() => patchSubtitle({ provider: "doubao" })}
                  >
                    豆包 · 火山
                  </button>
                </div>
                <div class="model-hint">
                  {v().subtitle.provider === "qwen"
                    ? "模型：qwen3-asr-flash-filetrans（异步转写 · 句级时间戳 · 最长 12 小时）"
                    : "模型：bigmodel 极速版 · volc.bigasr.auc_turbo（一次请求即返回）"}
                </div>
              </div>

              {/* 对应服务商凭证 */}
              <Show when={v().subtitle.provider === "qwen"}>
                <div class="settings-row">
                  <div class="settings-hint" style={{ "margin-bottom": "6px" }}>
                    DashScope API Key（百炼控制台获取）
                  </div>
                  <input
                    type="password"
                    class="cfg-input"
                    placeholder="sk-..."
                    value={v().subtitle.qwen_api_key}
                    onInput={(e) => patchSubtitle({ qwen_api_key: e.currentTarget.value })}
                  />
                </div>
              </Show>
              <Show when={v().subtitle.provider === "doubao"}>
                <div class="settings-row">
                  <div class="settings-hint" style={{ "margin-bottom": "6px" }}>
                    App Key（旧称 APP ID）
                  </div>
                  <input
                    type="password"
                    class="cfg-input"
                    placeholder="控制台获取的 App Key"
                    value={v().subtitle.doubao_app_key}
                    onInput={(e) => patchSubtitle({ doubao_app_key: e.currentTarget.value })}
                  />
                </div>
                <div class="settings-row">
                  <div class="settings-hint" style={{ "margin-bottom": "6px" }}>
                    Access Token（旧版控制台需要；新版留空）
                  </div>
                  <input
                    type="password"
                    class="cfg-input"
                    placeholder="新版控制台可留空"
                    value={v().subtitle.doubao_access_token}
                    onInput={(e) => patchSubtitle({ doubao_access_token: e.currentTarget.value })}
                  />
                </div>
              </Show>

              <div class="settings-row settings-row-inline">
                <div class="settings-hint">
                  按音频时长计费（约 0.8 元/小时），需网络请求；关掉则完全不发请求。
                </div>
                <div class="test-actions">
                  <button
                    class="settings-btn"
                    disabled={!credsReady(v().subtitle) || testState().kind === "running"}
                    onClick={() => void testConnection()}
                    title={credsReady(v().subtitle) ? "验证凭证与网络" : "请先填入凭证"}
                  >
                    {testState().kind === "running" ? "测试中…" : "测试连通"}
                  </button>
                  <button class="settings-btn primary" onClick={() => void saveSubtitle()}>
                    保存配置
                  </button>
                </div>
              </div>

              {/* 测试结果（idle 不显示） */}
              <Show when={testState().kind !== "idle"}>
                <div
                  class="test-result"
                  classList={{ ok: testState().kind === "ok", bad: testState().kind === "err" }}
                >
                  {testState().msg}
                </div>
              </Show>
            </section>

            {/* ---------- 录屏指针 ---------- */}
            <section class="settings-card">
              <div class="settings-row settings-row-inline">
                <div>
                  <div class="settings-label">在录屏中显示鼠标指针</div>
                  <div class="settings-hint">
                    屏幕采集本身不含指针，开启后会把系统真实指针图标原样合成进画面，便于回看点击与拖拽。
                  </div>
                </div>
                <button
                  class="switch"
                  classList={{ on: v().cursor_overlay }}
                  role="switch"
                  aria-checked={v().cursor_overlay}
                  aria-label="显示鼠标指针"
                  onClick={() => void toggleCursorOverlay(!v().cursor_overlay)}
                >
                  <span class="switch-knob" />
                </button>
              </div>
              <div class="settings-row settings-row-inline">
                <div>
                  <div class="settings-label">指针轮廓微光</div>
                  <div class="settings-hint">
                    沿指针图标的实际轮廓向外发散蓝色微光（斜箭头是斜光晕、I 型光标是细长光晕），
                    不改变指针本身的样子。
                  </div>
                </div>
                <button
                  class="switch"
                  classList={{ on: v().cursor_overlay && v().cursor_glow }}
                  disabled={!v().cursor_overlay}
                  role="switch"
                  aria-checked={v().cursor_glow}
                  aria-label="指针轮廓微光"
                  onClick={() => void toggleCursorGlow(!v().cursor_glow)}
                >
                  <span class="switch-knob" />
                </button>
              </div>
            </section>

            {/* ---------- 启动与诊断 ---------- */}
            <section class="settings-card">
              <div class="settings-row settings-row-inline">
                <div>
                  <div class="settings-label">开机自启</div>
                  <div class="settings-hint">开机后静默驻留托盘，等待快捷键唤起。</div>
                </div>
                <button
                  class="switch"
                  classList={{ on: v().autostart }}
                  role="switch"
                  aria-checked={v().autostart}
                  aria-label="开机自启"
                  onClick={() => void toggleAutostart(!v().autostart)}
                >
                  <span class="switch-knob" />
                </button>
              </div>
              <div class="settings-row settings-row-inline">
                <div>
                  <div class="settings-label">FFmpeg</div>
                  <div class="settings-hint">
                    {v().ffmpeg.found
                      ? `${v().ffmpeg.version || "已找到"}（来源：${originLabel(v().ffmpeg.origin)}）`
                      : "未找到：截图仍可用，但录制、导出、抽帧与场景检测不可用。"}
                  </div>
                  <Show when={!v().ffmpeg.found && v().ffmpeg.hint}>
                    <div class="settings-hint" style={{ "white-space": "pre-line", "margin-top": "6px" }}>
                      {v().ffmpeg.hint}
                    </div>
                  </Show>
                  <Show when={v().ffmpeg.found && v().ffmpeg.program}>
                    <div class="settings-hint">{v().ffmpeg.program}</div>
                  </Show>
                </div>
              </div>
              <div class="settings-row settings-row-inline">
                <div>
                  <div class="settings-label">运行日志</div>
                  <div class="settings-hint">{v().log_path || "（未启用）"}</div>
                </div>
                <button
                  class="settings-btn"
                  disabled={!v().log_path}
                  onClick={() => void revealItemInDir(v().log_path)}
                >
                  打开
                </button>
              </div>
            </section>

          </>
        )}
      </Show>

      {/* 提示信息必须放在 Show 之外：否则"读取设置失败"会被一直显示中的 loading 遮住 */}
      <Show when={msg()}>
        <div class="settings-msg ok">{msg()}</div>
      </Show>
      <Show when={err()}>
        <div class="settings-msg bad">{err()}</div>
      </Show>
    </div>
  );
}
