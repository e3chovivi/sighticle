import { createEffect, createSignal, onCleanup, onMount, For, Show } from "solid-js";
import type { JSX } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";
import "./App.css";

type Mode = "screenshot" | "record-mp4" | "camera";
/** pick（模式卡片）已移除：快捷键唤起即进入框选，模式在顶部条切换 */
type Stage = "select" | "annotate";
type Tool = "rect" | "arrow" | "pen" | "text" | "mosaic";

/// 工具栏小图标：内联 SVG，不引图标库（保持轻量）
const UI_ICONS: Record<string, JSX.Element> = {
  undo: (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
      <path d="M9 14 4 9l5-5" />
      <path d="M4 9h10a6 6 0 0 1 0 12h-3" />
    </svg>
  ),
  redo: (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
      <path d="m15 14 5-5-5-5" />
      <path d="M20 9H10a6 6 0 0 0 0 12h3" />
    </svg>
  ),
  close: (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round">
      <path d="M6 6l12 12M18 6 6 18" />
    </svg>
  ),
  download: (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
      <path d="M12 3v12" />
      <path d="m7 11 5 5 5-5" />
      <path d="M4 21h16" />
    </svg>
  ),
  check: (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="m4 12.5 5.5 5.5L20 7" />
    </svg>
  ),
};

interface Pos {
  x: number;
  y: number;
}

interface Region {
  x: number;
  y: number;
  w: number;
  h: number;
}

interface Shape {
  type: Tool;
  color: string;
  size: number;
  points: Pos[];
  text?: string;
}

const COLORS = ["#ff4d4f", "#ffb400", "#22c55e", "#3b82f6", "#a855f7", "#ffffff"];
const TOOLS: { id: Tool; label: string }[] = [
  { id: "rect", label: "矩形" },
  { id: "arrow", label: "箭头" },
  { id: "pen", label: "画笔" },
  { id: "text", label: "文字" },
  { id: "mosaic", label: "马赛克" },
];

export default function App() {
  // 唤起即框选：默认截图模式，录屏/摄像头在顶部条切换
  const [stage, setStage] = createSignal<Stage>("select");
  const [mode, setMode] = createSignal<Mode>("screenshot");
  const [start, setStart] = createSignal<Pos | null>(null);
  const [cur, setCur] = createSignal<Pos>({ x: 0, y: 0 });
  const [region, setRegion] = createSignal<Region | null>(null);

  // ---- 截图标注状态 ----
  const [shot, setShot] = createSignal<{ width: number; height: number; data_url: string } | null>(null);
  const [tool, setTool] = createSignal<Tool>("rect");
  const [color, setColor] = createSignal(COLORS[0]);
  const [shapes, setShapes] = createSignal<Shape[]>([]);
  const [redoStack, setRedoStack] = createSignal<Shape[]>([]);
  const [drawing, setDrawing] = createSignal(false);
  const [textInput, setTextInput] = createSignal<Pos | null>(null);
  const [textValue, setTextValue] = createSignal("");
  const [toast, setToast] = createSignal("");
  // 音频开关（本地持久化）：默认关闭，需在模式选择处显式开启
  const [micOn, setMicOn] = createSignal(localStorage.getItem("lc-mic") === "1");
  const [sysOn, setSysOn] = createSignal(localStorage.getItem("lc-sys") === "1");

  function toggleAudio(which: "mic" | "sys") {
    if (which === "mic") {
      const v = !micOn();
      setMicOn(v);
      localStorage.setItem("lc-mic", v ? "1" : "0");
    } else {
      const v = !sysOn();
      setSysOn(v);
      localStorage.setItem("lc-sys", v ? "1" : "0");
    }
  }
  let canvasRef!: HTMLCanvasElement;
  let baseImg: HTMLImageElement | null = null;
  let baseCanvas: HTMLCanvasElement | null = null;

  const rect = () => {
    const s = start();
    if (!s) return null;
    const c = cur();
    return {
      x: Math.min(s.x, c.x),
      y: Math.min(s.y, c.y),
      w: Math.abs(s.x - c.x),
      h: Math.abs(s.y - c.y),
    };
  };

  function showToast(msg: string) {
    setToast(msg);
    setTimeout(() => setToast(""), 2600);
  }

  function choose(m: Mode) {
    setMode(m);
    // 摄像头没有框选概念：点击即开始录制
    if (m === "camera") {
      void beginRecording(m, { x: 0, y: 0, w: 0, h: 0 });
    }
  }

  /// 回到框选阶段（覆盖层保持可见）：标注取消 / 重新选择时用
  function resetToSelect() {
    setStage("select");
    setStart(null);
    setRegion(null);
    setShapes([]);
    setRedoStack([]);
    setHoverWin(null);
    baseImg = null;
    windowsLoaded = false; // 下次框选重新枚举窗口（位置可能已变化）
    void loadWindows();
  }

  async function cancel() {
    await invoke("hide_overlay");
    resetToSelect();
  }

  // ================= 框选阶段（飞书式智能选区） =================
  interface WinRect {
    title: string;
    x: number;
    y: number;
    width: number;
    height: number;
  }
  const [winRects, setWinRects] = createSignal<WinRect[]>([]);
  const [hoverWin, setHoverWin] = createSignal<WinRect | null>(null);
  let windowsLoaded = false;

  async function loadWindows() {
    if (windowsLoaded) return;
    try {
      const list = await invoke<WinRect[]>("list_windows");
      setWinRects(list);
      windowsLoaded = true;
    } catch {
      /* 识别失败不影响手动框选 */
    }
  }

  /// 命中该点的最上层窗口。winRects 保持 Z 序（最上层在前，见后端 list_windows），
  /// 因此取第一个包含该点的窗口就是用户视觉上看到的那个——比"最小面积"启发式准：
  /// 重叠窗口不再误选被压在下面的小窗口，悬停高亮也不会在两个候选间来回跳。
  /// 桌面/任务栏等"空白"窗口命中即视为不可选（单击这些区域应当退出），
  /// 也不透传到它背后的窗口——用户看到的是空白层，选中背后窗口必然"捕获不准"。
  function hitWindow(x: number, y: number): WinRect | null {
    const BLANK_TITLES = ["Program Manager", "任务栏", "Taskbar", "Search"];
    for (const w of winRects()) {
      if (x >= w.x && x <= w.x + w.width && y >= w.y && y <= w.y + w.height) {
        return BLANK_TITLES.includes(w.title) ? null : w;
      }
    }
    return null;
  }

  async function commitRegion(r: Region) {
    setRegion(r);
    const m = mode();
    if (m === "screenshot") {
      await beginScreenshot(r);
    } else {
      await beginRecording(m, r);
    }
  }

  function onMouseDown(e: MouseEvent) {
    const t = e.target as HTMLElement;
    // 浮层 UI（顶部模式条/标注工具栏）上的按下不是框选手势，必须忽略：
    // 否则点「录屏」tab 会先被当成"单击选窗口"触发截图/录屏（mouseup 早于
    // click，用的还是切换前的旧模式），模式切换反而排在其后，表现为"切错模式"。
    if (t.closest(".top-bar, .toolbar")) return;
    // 标注中：点击选区外的背景 = 直接退出（画布/工具栏/文字输入内部不受影响）
    if (stage() === "annotate") {
      if (!t.closest(".annotate-wrap, .toolbar, .toast")) void cancel();
      return;
    }
    if (stage() !== "select") return;
    if (e.button === 2) {
      void cancel();
      return;
    }
    if (e.button !== 0) return;
    setStart({ x: e.clientX, y: e.clientY });
    setCur({ x: e.clientX, y: e.clientY });
  }

  function onMouseMove(e: MouseEvent) {
    if (stage() !== "select") return;
    setCur({ x: e.clientX, y: e.clientY });
    // 未按下拖拽时做悬停窗口识别
    if (!start()) setHoverWin(hitWindow(e.clientX, e.clientY));
  }

  async function onMouseUp(e: MouseEvent) {
    const t = e.target as HTMLElement;
    const s = start();
    if (!s || e.button !== 0 || stage() !== "select") return;
    const r = rect();
    setStart(null);
    // 单击（几乎没拖动）：命中窗口选窗口；点在空白（含桌面）= 直接退出
    if (!r || r.w < 4 || r.h < 4) {
      // 松手落在浮层 UI 上的"点击"同样不是框选手势（按下在背景、松开在模式条）
      if (t.closest(".top-bar, .toolbar")) return;
      const hit = hitWindow(e.clientX, e.clientY);
      if (hit) {
        await commitRegion({ x: hit.x, y: hit.y, w: hit.width, h: hit.height });
      } else {
        await cancel();
      }
      return;
    }
    await commitRegion(r);
  }

  // ================= 截图 =================
  async function beginScreenshot(r: Region) {
    try {
      const data = await invoke<{ width: number; height: number; data_url: string }>(
        "capture_region_screenshot",
        { region: { x: r.x, y: r.y, width: r.w, height: r.h } }
      );
      // 先把底图解码好并画上画布，再切进标注界面并恢复覆盖层：
      // 若先切界面再异步加载图片，会看到"空画布顿一下、图片随后弹出"的顿挫感。
      const img = await loadImage(data.data_url);
      baseImg = img;
      // 离屏副本供马赛克采样（需读未叠加标注的原始像素）
      const off = document.createElement("canvas");
      off.width = img.width;
      off.height = img.height;
      off.getContext("2d")!.drawImage(img, 0, 0);
      baseCanvas = off;
      setShot(data);
      setShapes([]);
      setRedoStack([]);
      setTool("rect");
      setStage("annotate");
      redraw();
      // 不支持采集排除的系统上，后端为拍出干净画面已把覆盖层隐藏（否则会拍进紫色蒙版），
      // 这里恢复显示；采集排除生效时覆盖层本就可见，这次调用是无副作用的空操作。
      await invoke("show_overlay");
    } catch (err) {
      // 后端为拍出干净画面可能先隐藏了覆盖层；失败时务必恢复界面，否则用户会面对"消失的界面"
      await invoke("show_overlay").catch(() => {});
      showToast(`截图失败：${err}`);
      resetToSelect();
    }
  }

  /// 解码截图底图：解码完成才返回，保证切进标注界面的同一帧画布上已有内容
  function loadImage(src: string): Promise<HTMLImageElement> {
    return new Promise((resolve, reject) => {
      const img = new Image();
      img.onload = () => {
        // 显式解码：避免后面首次 drawImage 在主线程触发同步解码而掉帧
        img.decode().then(
          () => resolve(img),
          () => resolve(img)
        );
      };
      img.onerror = () => reject(new Error("截图解码失败"));
      img.src = src;
    });
  }

  function canvasScale() {
    const r = region();
    const s = shot();
    if (!r || !s) return 1;
    return s.width / r.w;
  }

  function toCanvas(e: MouseEvent): Pos {
    const r = region()!;
    const sc = canvasScale();
    return { x: (e.clientX - r.x) * sc, y: (e.clientY - r.y) * sc };
  }

  function redraw() {
    const cv = canvasRef;
    const s = shot();
    if (!cv || !s) return;
    const ctx = cv.getContext("2d")!;
    ctx.clearRect(0, 0, cv.width, cv.height);
    if (baseImg) ctx.drawImage(baseImg, 0, 0, cv.width, cv.height);
    for (const sh of shapes()) drawShape(ctx, sh);
  }

  function drawShape(ctx: CanvasRenderingContext2D, sh: Shape) {
    ctx.save();
    ctx.strokeStyle = sh.color;
    ctx.fillStyle = sh.color;
    ctx.lineWidth = sh.size;
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    const pts = sh.points;
    if (sh.type === "rect" && pts.length >= 2) {
      const [a, b] = [pts[0], pts[pts.length - 1]];
      ctx.strokeRect(a.x, a.y, b.x - a.x, b.y - a.y);
    } else if (sh.type === "arrow" && pts.length >= 2) {
      const a = pts[0];
      const b = pts[pts.length - 1];
      ctx.beginPath();
      ctx.moveTo(a.x, a.y);
      ctx.lineTo(b.x, b.y);
      ctx.stroke();
      // 箭头
      const ang = Math.atan2(b.y - a.y, b.x - a.x);
      const len = 12 + sh.size * 2;
      ctx.beginPath();
      ctx.moveTo(b.x, b.y);
      ctx.lineTo(b.x - len * Math.cos(ang - 0.4), b.y - len * Math.sin(ang - 0.4));
      ctx.moveTo(b.x, b.y);
      ctx.lineTo(b.x - len * Math.cos(ang + 0.4), b.y - len * Math.sin(ang + 0.4));
      ctx.stroke();
    } else if (sh.type === "pen" && pts.length >= 2) {
      ctx.beginPath();
      ctx.moveTo(pts[0].x, pts[0].y);
      for (let i = 1; i < pts.length; i++) ctx.lineTo(pts[i].x, pts[i].y);
      ctx.stroke();
    } else if (sh.type === "text" && sh.text) {
      ctx.font = `${16 * canvasScale()}px "PingFang SC", "Microsoft YaHei", sans-serif`;
      ctx.fillText(sh.text, pts[0].x, pts[0].y);
    } else if (sh.type === "mosaic") {
      const block = Math.max(6, 8 * canvasScale());
      const baseCtx = baseCanvas?.getContext("2d") ?? null;
      for (const p of pts) {
        const bx = Math.floor(p.x / block) * block;
        const by = Math.floor(p.y / block) * block;
        if (baseCtx) {
          // 取块左上像素近似块平均色（离屏采样，避免读主画布已叠加的标注）
          const d = baseCtx.getImageData(
            Math.min(bx, baseCanvas!.width - 1),
            Math.min(by, baseCanvas!.height - 1),
            1,
            1
          ).data;
          ctx.fillStyle = `rgb(${d[0]},${d[1]},${d[2]})`;
        }
        ctx.fillRect(bx, by, block, block);
      }
    }
    ctx.restore();
  }

  function onCanvasDown(e: MouseEvent) {
    if (stage() !== "annotate") return;
    e.stopPropagation();
    if (tool() === "text") {
      setTextInput(toCanvas(e));
      setTextValue("");
      return;
    }
    setDrawing(true);
    const p = toCanvas(e);
    setShapes((prev) => [...prev, { type: tool(), color: color(), size: 3 * canvasScale(), points: [p] }]);
    setRedoStack([]);
  }

  function onCanvasMove(e: MouseEvent) {
    if (!drawing() || stage() !== "annotate") return;
    const p = toCanvas(e);
    setShapes((prev) => {
      const copy = [...prev];
      const last = copy[copy.length - 1];
      if (last) last.points = [...last.points, p];
      return copy;
    });
    redraw();
  }

  function onCanvasUp() {
    if (!drawing()) return;
    setDrawing(false);
    redraw();
  }

  function commitText() {
    const pos = textInput();
    const val = textValue().trim();
    if (pos && val) {
      setShapes((prev) => [...prev, { type: "text", color: color(), size: 3, points: [pos], text: val }]);
      setRedoStack([]);
      redraw();
    }
    setTextInput(null);
    setTextValue("");
  }

  function undo() {
    setShapes((prev) => {
      if (prev.length === 0) return prev;
      const copy = [...prev];
      const last = copy.pop()!;
      setRedoStack((r) => [...r, last]);
      return copy;
    });
    redraw();
  }

  function redo() {
    setRedoStack((prev) => {
      if (prev.length === 0) return prev;
      const copy = [...prev];
      const last = copy.pop()!;
      setShapes((s) => [...s, last]);
      return copy;
    });
    redraw();
  }

  function compositedDataUrl(): string {
    const cv = canvasRef;
    return cv.toDataURL("image/png");
  }

  async function confirmCopy() {
    try {
      await invoke("copy_image_to_clipboard", { dataUrl: compositedDataUrl() });
      showToast("已复制到剪贴板");
      await invoke("hide_overlay");
      resetToSelect();
    } catch (err) {
      showToast(`复制失败：${err}`);
    }
  }

  function stampName() {
    return `截图_${new Date().toISOString().slice(0, 19).replace(/[-:T]/g, "")}.png`;
  }

  /// 保存截图，两条路径：
  /// 1) 设置了默认保存目录 → 直接存进去并给出提示，不再弹框；
  /// 2) 未设置 → 弹「另存为」对话框。**弹框前必须先收起覆盖层**：它是
  ///    always_on_top 的全屏窗口，会把系统对话框盖在下面，用户看不到
  ///    对话框，看起来就像"保存提示没有出现"（同坑 1 的教训）。
  async function confirmSave() {
    let saveDir = "";
    try {
      const cfg = await invoke<{ save_dir: string }>("get_settings");
      saveDir = (cfg.save_dir ?? "").trim();
    } catch {
      /* 读不到设置就按「每次询问」处理 */
    }

    if (saveDir) {
      try {
        const dir = saveDir.replace(/[\\/]+$/, "");
        const path = `${dir}\\${stampName()}`;
        await invoke("save_image", { dataUrl: compositedDataUrl(), path });
        showToast(`已保存：${path}`);
        await new Promise((r) => setTimeout(r, 900)); // 让提示停留一下再收界面
        await invoke("hide_overlay").catch(() => {});
        resetToSelect();
      } catch (err) {
        showToast(`保存失败：${err}`);
      }
      return;
    }

    await invoke("hide_overlay").catch(() => {});
    try {
      const path = await save({
        defaultPath: stampName(),
        filters: [{ name: "PNG 图片", extensions: ["png"] }],
        title: "保存截图",
      });
      if (!path) {
        // 用户取消：把标注界面还回来，标注内容保留
        await invoke("show_overlay").catch(() => {});
        return;
      }
      await invoke("save_image", { dataUrl: compositedDataUrl(), path });
      resetToSelect(); // 覆盖层保持隐藏，下次唤起回到模式选择
    } catch (err) {
      await invoke("show_overlay").catch(() => {});
      showToast(`保存失败：${err}`);
    }
  }

  // ================= 录屏 =================
  // 录制启动后：后端隐藏全屏覆盖层并显示独立控制条窗口（RecBar），
  // 录制期间鼠标可自由操作电脑；停止/保存/结果展示都在控制条窗口内完成。
  async function beginRecording(m: Mode, r: Region) {
    try {
      await invoke("start_recording", {
        mode: m,
        region: { x: r.x, y: r.y, width: r.w, height: r.h },
        mic: micOn(),
        sysAudio: sysOn(),
      });
      // 覆盖层已被后端隐藏；重置本地状态，下次唤起回到模式选择
      resetToSelect();
    } catch (err) {
      // 启动失败时后端已把覆盖层还回来；这里再兜一次，避免 IPC 异常导致界面消失
      await invoke("show_overlay").catch(() => {});
      showToast(`开始录制失败：${err}`);
      resetToSelect();
    }
  }

  // 打开设置窗口。
  // 必须先收起覆盖层：它是全屏置顶窗口，设置窗口会被它挡在后面，整个桌面也点不动。
  // 万一创建失败再把覆盖层还回来，避免界面"消失"让人以为程序挂了。
  async function openSettings() {
    await invoke("hide_overlay").catch(() => {});
    try {
      await invoke("open_settings");
    } catch (err) {
      await invoke("show_overlay").catch(() => {});
      showToast(`打开设置失败：${err}`);
    }
  }

  function onKeyDown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      if (stage() === "annotate") {
        resetToSelect();
      } else {
        void cancel();
      }
      return;
    }
    if (stage() === "select") {
      // 1/2/3 快速切换模式（3=摄像头，点击即录）
      if (e.key === "1") choose("screenshot");
      if (e.key === "2") choose("record-mp4");
      if (e.key === "3") choose("camera");
    }
    if (stage() === "annotate") {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "z") {
        e.preventDefault();
        e.shiftKey ? redo() : undo();
      }
      if (e.key === "Enter") {
        e.preventDefault();
        void confirmCopy();
      }
    }
  }

  const preventContextMenu = (e: MouseEvent) => e.preventDefault();

  let unlistenOverlayShown: (() => void) | null = null;
  onMount(() => {
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("contextmenu", preventContextMenu);
    void loadWindows(); // 唤起即框选，窗口识别直接开始
    // 覆盖层每次显示都刷新窗口快照：应用启动时加载的那份会随时间失效
    // （新开的窗口、挪过位置的窗口都不在里面），是"自动捕获不准"的主因。
    void listen("overlay-shown", () => {
      windowsLoaded = false;
      void loadWindows();
    }).then((un) => {
      unlistenOverlayShown = un;
    });
  });
  onCleanup(() => {
    window.removeEventListener("keydown", onKeyDown);
    window.removeEventListener("contextmenu", preventContextMenu);
    unlistenOverlayShown?.();
  });

  const regionStyle = () => {
    const r = region();
    if (!r) return {};
    return { left: `${r.x}px`, top: `${r.y}px`, width: `${r.w}px`, height: `${r.h}px` };
  };

  /// 浮层（工具栏/尺寸标签）的可视边界：任务栏会盖住全屏覆盖层的底边
  ///（taskbar 是 topmost），所以除了窗口尺寸还要减去任务栏区域（availHeight，
  /// 按主显示器计——本应用所有浮层都落在主屏）。
  function visibleBounds() {
    const right = Math.min(window.innerWidth, screen.availWidth) - 8;
    const bottom = Math.min(window.innerHeight, screen.availHeight) - 8;
    return { right: Math.max(right, 8), bottom: Math.max(bottom, 8) };
  }

  // ---- 标注工具栏定位：选区贴屏幕边缘时工具栏要翻转/夹进可视区，否则按钮被裁掉 ----
  let toolbarRef!: HTMLDivElement;
  const [tbSize, setTbSize] = createSignal({ w: 0, h: 0 });
  createEffect(() => {
    if (stage() !== "annotate" || !region()) return;
    const el = toolbarRef;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    if (w > 0 && (w !== tbSize().w || h !== tbSize().h)) setTbSize({ w, h });
  });

  const toolbarStyle = () => {
    const r = region();
    if (!r) return {};
    const { w, h } = tbSize();
    const { right, bottom } = visibleBounds();
    let left = Math.max(8, r.x);
    let top = r.y + r.h + 12;
    if (h > 0) {
      // 选区贴底边（工具栏放不下，会压到任务栏下面）→ 翻到选区上方
      if (top + h > bottom) {
        top = r.y - h - 12;
        if (top < 8) top = Math.max(8, Math.min(r.y + r.h + 12, bottom - h));
      }
      if (left + w > right) left = Math.max(8, right - w);
    } else {
      // 首帧未测量：先粗略右夹，紧接着的测量回调会精确修正
      if (left + 720 > right) left = Math.max(8, right - 720);
    }
    return { left: `${left}px`, top: `${top}px` };
  };

  /// 尺寸标签：同样的边缘问题，贴右/贴底时翻进选区内
  const sizeTagStyle = (r: { x: number; y: number; w: number; h: number }) => {
    const { right, bottom } = visibleBounds();
    const W = 92;
    const H = 26;
    let left = r.x + r.w + 8;
    let top = r.y + r.h + 8;
    if (left + W > right) left = Math.max(8, r.x + r.w - W);
    if (top + H > bottom) top = Math.max(8, r.y + r.h - H);
    return { left: `${left}px`, top: `${top}px` };
  };

  return (
    <div
      class="overlay"
      classList={{
        selecting: stage() === "select",
      }}
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={onMouseUp}
    >
      <Show when={toast()}>
        <div class="toast">{toast()}</div>
      </Show>

      {/* ---------- 框选（唤起即进入，顶部条切换模式） ---------- */}
      <Show when={stage() === "select"}>
        <div class="top-bar">
          <div class="mode-tabs">
            <button class="mode-tab" classList={{ on: mode() === "screenshot" }} onClick={() => choose("screenshot")} title="截图（1）">
              截图
            </button>
            <button class="mode-tab" classList={{ on: mode() === "record-mp4" }} onClick={() => choose("record-mp4")} title="录屏（2）">
              录屏
            </button>
            <button class="mode-tab" onClick={() => choose("camera")} title="录制摄像头，点击即录（3）">
              摄像头
            </button>
          </div>
          <Show when={mode() !== "screenshot"}>
            <span class="top-sep" />
            <button class="audio-chip" classList={{ on: micOn() }} onClick={() => toggleAudio("mic")} title="录制麦克风">
              麦克风
            </button>
            <button class="audio-chip" classList={{ on: sysOn() }} onClick={() => toggleAudio("sys")} title="录制系统声音">
              系统声音
            </button>
          </Show>
          <span class="top-sep" />
          <button class="settings-link" onClick={() => void openSettings()}>
            设置
          </button>
        </div>
        <div class="hint">拖拽框选 · 单击选窗口 · 单击空白退出 · Esc 取消</div>
        <Show when={!start() && hoverWin()} keyed>
          {(hw) => (
            <div
              class="hover-win"
              style={{ left: `${hw.x}px`, top: `${hw.y}px`, width: `${hw.width}px`, height: `${hw.height}px` }}
            >
              <span class="hover-win-title">{hw.title}</span>
            </div>
          )}
        </Show>
        <div class="cross v" style={{ left: `${cur().x}px` }} />
        <div class="cross h" style={{ top: `${cur().y}px` }} />
        <Show when={rect()}>
          {(r) => (
            <>
              <div
                class="region"
                style={{ left: `${r().x}px`, top: `${r().y}px`, width: `${r().w}px`, height: `${r().h}px` }}
              />
              <div class="size-tag" style={sizeTagStyle(r())}>
                {Math.round(r().w)} × {Math.round(r().h)}
              </div>
            </>
          )}
        </Show>
      </Show>

      {/* ---------- 截图标注 ---------- */}
      <Show when={stage() === "annotate" && shot()}>
        <div class="annotate-wrap" style={regionStyle()}>
          <canvas
            ref={canvasRef}
            width={shot()?.width ?? 0}
            height={shot()?.height ?? 0}
            class="annotate-canvas"
            onMouseDown={onCanvasDown}
            onMouseMove={onCanvasMove}
            onMouseUp={onCanvasUp}
          />
          <Show when={textInput()}>
            <input
              class="text-input"
              style={{ left: `${textInput()!.x / canvasScale()}px`, top: `${textInput()!.y / canvasScale()}px`, color: color() }}
              value={textValue()}
              autofocus
              onInput={(e) => setTextValue(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitText();
                if (e.key === "Escape") setTextInput(null);
              }}
              onBlur={commitText}
            />
          </Show>
        </div>

        {/* 工具栏（贴边时自动翻转/夹进可视区，见 toolbarStyle） */}
        <div
          ref={toolbarRef}
          class="toolbar"
          style={toolbarStyle()}
          onMouseDown={(e) => e.stopPropagation()}
        >
          <For each={TOOLS}>
            {(t) => (
              <button class="tool-btn" classList={{ active: tool() === t.id }} onClick={() => setTool(t.id)} title={t.label}>
                {t.label}
              </button>
            )}
          </For>
          <span class="toolbar-sep" />
          <For each={COLORS}>
            {(c) => (
              <button
                class="color-dot"
                classList={{ active: color() === c }}
                style={{ background: c }}
                onClick={() => setColor(c)}
              />
            )}
          </For>
          <span class="toolbar-sep" />
          <button class="tool-btn" disabled={shapes().length === 0} onClick={undo} title="撤销 (Ctrl+Z)" aria-label="撤销">
            {UI_ICONS.undo}
          </button>
          <button class="tool-btn" disabled={redoStack().length === 0} onClick={redo} title="重做 (Ctrl+Shift+Z)" aria-label="重做">
            {UI_ICONS.redo}
          </button>
          <span class="toolbar-sep" />
          <button class="action-btn cancel" onClick={() => resetToSelect()} title="取消 (Esc)" aria-label="取消">
            {UI_ICONS.close}
            <span class="action-text">取消</span>
          </button>
          <button class="action-btn save" onClick={() => void confirmSave()} title="保存为 PNG" aria-label="保存">
            {UI_ICONS.download}
            <span class="action-text">保存</span>
          </button>
          <button class="action-btn confirm" onClick={() => void confirmCopy()} title="复制 (Enter)" aria-label="复制">
            {UI_ICONS.check}
            <span class="action-text">复制</span>
          </button>
        </div>
        <div class="hint annotate-hint">Enter 复制 · Ctrl+Z 撤销 · 点击外部或 Esc 退出</div>
      </Show>

      {/* 录屏中 / 录制结果：已迁移到独立控制条窗口（RecBar），录制期间不遮挡屏幕 */}
    </div>
  );
}
