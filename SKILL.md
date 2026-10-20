---
name: light-capture
description: Windows 轻量屏幕截图与录屏工具（CLI 无头模式）。当需要截取屏幕、录制屏幕/系统声音/麦克风/摄像头、或用户要求「截图」「录屏」「录制这段操作」时调用。零常驻、无窗口、单行 JSON 输出。
---

# light-capture 能力声明

给 AI Agent 的工具说明书：什么时候用、怎么调、怎么停、结果怎么看。
机器可读版本执行 `light-capture help --json` 获取（与本文件同源）。

## 触发时机

- 用户要求**截图 / 截屏 / 抓取屏幕画面**时
- 用户要求**录屏 / 录制操作过程 / 录制视频**时
- 需要采集**系统声音、麦克风语音、摄像头画面**时
- 不要用于：编辑图片/视频、处理已存在的媒体文件（用其他工具）

## 能力与命令

### 1. 截图

```text
light-capture screenshot --out <path.png> [--region x,y,w,h]
```

- `--out`（必填）：PNG 输出路径
- `--region`（可选）：全局物理像素 `x,y,w,h`；缺省 = 主显示器整屏
- 返回：`{"ok":true,"path":"...","width":...,"height":...,"region":{...}}`

### 2. 录屏

```text
light-capture record [--seconds <n>] --out <path.mp4|.gif> [--region x,y,w,h] [--window <title>] [--camera] [--mic] [--sys-audio] [--stop-file <path>] [--fps <n>] [--scale <w>] [--crf <n>] [--no-cursor] [--no-glow]
```

- `--out`（必填）：`.mp4` 或 `.gif`（后缀决定格式）
- `--seconds`：录制时长，0.2 ~ 3600；**与 `--stop-file` 至少给一个**
- `--stop-file`：**随时停止**——录制期间创建该文件即立刻结束（见下文）
- `--region`：屏幕选区；`--camera` 录摄像头（与 `--region` 互斥）
- `--window`：**按窗口标题录制**（子串匹配、不区分大小写），无需自己算坐标；与 `--region` 互斥
- `--mic` / `--sys-audio`：录麦克风 / 系统声音（可同开）
- `--fps`：输出帧率 1~60，默认 30；**回看交互时 15 通常足够**
- `--scale`：输出宽度上限（高度按比例），默认原始分辨率；**1280 在多数 UI 复盘场景已够用**
- `--crf`：x264 画质 0~51，默认 23，越大文件越小；**28 可明显减小体积**
- `--no-cursor`：不把鼠标指针画进画面（**默认会画**——使用系统真实指针图标，AI 回看时才能看出点在哪里、当前是什么操作状态）
- `--no-glow`：不绘制沿指针轮廓发散的蓝色微光（**默认绘制**，用于在任意背景上突出指针）
- 返回：`{"ok":true,"path":"...","seconds":实际时长,"window":{...},"fps":15,"cursor":true,"stopped_by":"stop_file|duration",...}`

### 2.1 列出窗口（按标题录制前先查）

```text
light-capture list-windows
```

返回当前可见窗口的标题、进程名与物理像素坐标（最上层在前），配合 `--window` 使用。

### 3. 分析视频：时间轴 + 关键帧 + 拼图

```text
light-capture analyze --in <video.mp4> [--frames <n>] [--sheet <path>] [--cols <n>] [--no-scenes] [--scene-threshold <f>] [--scale <w>]
```

返回 `timeline` 对象：

| 字段 | 含义 |
| --- | --- |
| `duration` | 时长（秒） |
| `width` / `height` / `fps` | 分辨率与帧率 |
| `scene_cuts` | 场景切换点 `[{t, score}]`，`score` 越大变化越剧烈 |
| `segments` | 相邻切换点之间的片段 `[{start, end, duration}]` |
| `frames` / `frame_times` | 已抽取的关键帧路径与对应时刻（传了 `--frames` 才有） |
| `sheet` | 拼图路径（传了 `--sheet` 才有） |

- `--frames`：均匀抽取 1~120 张关键帧，默认目录为视频同级的 `<文件名>.frames/`
- `--sheet`：把多帧平铺成一张图，适合一次性交给多模态模型
- `--cols`：拼图列数，默认 4
- `--no-scenes`：跳过场景检测（长视频可省时间）
- `--scene-threshold`：切换灵敏度，默认 0.30

**推荐用法**：先不带参数跑一次 analyze 拿到时间轴，再按 `segments` 决定看哪几段；
需要判断 UI 细节时用 `--frames 6 --sheet`，比让模型看完整视频更准也更省 token。

录制时也可直接带上这些参数（`record ... --frames 6 --sheet out.jpg --scenes`），
录像结束时同一个 JSON 里会多出 `timeline`，不必再调一次 analyze。

### 4. 环境自检（建议首次调用前先查）

```text
light-capture doctor
```

返回 `ffmpeg` 对象：`found`、`program`（路径）、`origin`（env / exe_dir / path / well_known）、`version`，缺失时给出 `error` 与可直接复制的 `hint`。

- **截图不需要 FFmpeg**；录制、导出、抽帧、场景检测需要。
- 找到后可用环境变量 `LIGHT_CAPTURE_FFMPEG` 指定自定义路径（文件或所在目录）。
- 找不到时按提示安装：`winget install --id Gyan.FFmpeg`，或把 `ffmpeg.exe` 放到主程序同目录。

### 5. 帮助

```text
light-capture help          # 人类可读
light-capture help --json   # 机器可读能力声明（推荐先读这个）
```

## 如何随时停止录屏（AI 关键流程）

已知时长 → 直接 `--seconds`。**不确定要录多久** → 用停止文件：

1. 发起录制（后台运行，不加 `--seconds` 或给个大上限）：
   ```text
   light-capture record --stop-file D:/rec/stop.flag --out D:/rec/demo.mp4 --mic
   ```
2. 等待用户/流程认为内容够了（100ms 级响应）
3. 创建停止文件即停止：`echo stop > D:/rec/stop.flag`（PowerShell：`New-Item -ItemType File -Force D:\rec\stop.flag`）
4. 命令退出时输出一行 JSON，`stopped_by` 为 `"stop_file"`，停止文件已被自动清理
5. 按返回的 `path` 读取/继续处理视频

注意：停止文件**开录时自动删除残留**，直接创建即可，无需先清理。

## 输出契约

- **stdout 恰好一行 JSON**：成功 `{"ok":true,...}`，失败 `{"ok":false,"error":"原因"}`
- 退出码：`0` 成功 / `1` 失败
- 详细过程日志写本地日志文件，不污染 stdout

## 约束

- Windows 专有（WGC 屏幕采集 / WASAPI 系统声音），不支持 macOS
- 不建窗口、不注册热键/托盘，可与 GUI 实例并存互不干扰
- 零常驻：进程退出即释放全部资源
- 可执行文件位置：安装或加入 PATH 后直接用 `light-capture`；未安装时为仓库内构建产物 `src-tauri\target\release\light-capture.exe`（Windows，无头 CLI，启动不弹窗口）
