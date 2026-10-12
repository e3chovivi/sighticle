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
light-capture record [--seconds <n>] --out <path.mp4|.gif> [--region x,y,w,h] [--camera] [--mic] [--sys-audio] [--stop-file <path>]
```

- `--out`（必填）：`.mp4` 或 `.gif`（后缀决定格式）
- `--seconds`：录制时长，0.2 ~ 3600；**与 `--stop-file` 至少给一个**
- `--stop-file`：**随时停止**——录制期间创建该文件即立刻结束（见下文）
- `--region`：屏幕选区；`--camera` 录摄像头（与 `--region` 互斥）
- `--mic` / `--sys-audio`：录麦克风 / 系统声音（可同开）
- 返回：`{"ok":true,"path":"...","seconds":实际时长,"stopped_by":"stop_file|duration",...}`

### 3. 帮助

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
- 详细过程日志写 D 盘日志文件，不污染 stdout

## 约束

- Windows 专有（WGC 屏幕采集 / WASAPI 系统声音），不支持 macOS
- 不建窗口、不注册热键/托盘，可与 GUI 实例并存互不干扰
- 零常驻：进程退出即释放全部资源
- 可执行文件位置：安装后为 `light-capture.exe`；开发时在 `src-tauri/target/debug/light-capture.exe`
