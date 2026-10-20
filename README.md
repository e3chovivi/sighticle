<div align="center">

# light-capture

**轻量级 Windows 录屏 / 截图工具 —— 让 AI Agent 能够回看自己的执行过程**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%2010%2B-0078d4)
![Built with](https://img.shields.io/badge/built%20with-Tauri%202%20%2B%20Rust-24c8db)

简体中文 | [English](README_EN.md)

</div>

---

## 解决什么问题

AI Agent 正在承担越来越多的自动化工作：软件测试、RPA 流程、视频素材处理等。这类任务有一个共同的缺口——Agent 完成操作后，缺少确认实际执行效果的手段：截图只能反映某一瞬间，文字日志缺少画面信息。

视频同时包含画面、时间与节奏，是目前信息密度最高的记录方式。light-capture 将录屏能力以一条命令的形式提供给 AI：Agent 自主录制屏幕、取回视频文件，再交给多模态模型回看分析，构成完整的「执行 → 录制 → 回看 → 校正」闭环。

## 工作方式

```text
① 录    Agent 执行一条命令开始录屏，期间可随时中止
② 收    录制结束，stdout 返回一行 JSON，包含视频文件路径
③ 看    Agent 将视频交给多模态模型回看，据此决定下一步
```

```bash
# 发起录制：不设固定时长，用"停止文件"随时中止
light-capture record --stop-file D:/rec/stop.flag --out D:/rec/demo.mp4

# （Agent 认为录制足够时，创建 stop.flag 文件，录制立即结束）

# 进程退出，stdout 恰好一行 JSON：
{"ok":true,"path":"D:/rec/demo.mp4","seconds":38.2,"stopped_by":"stop_file",...}
```

视频就是普通的本地文件：无平台绑定、无账号，默认也不会离开本机，Agent 可以将其交给任意支持视频输入的多模态模型。

## 核心特性

- **极致轻量** —— AI Agent 普遍消耗大量硬件资源，本工具刻意做减法：CLI 模式不建窗口、不注册热键、进程退出即释放全部资源，空闲时零消耗，录制时只用必要的编码资源；GUI 模式才常驻托盘以承接全局快捷键
- **指针可视化** —— 屏幕采集本身不含鼠标指针；本工具会把**系统真实指针图标原样**合成进画面（箭头、I 型、手型等保持系统样式），并沿图标自身的轮廓向外发散一层蓝色微光，让 AI 一眼看出点击与拖拽发生在哪里（可用 `--no-cursor` / `--no-glow` 关闭）
- **录屏与视频选段** —— 支持区域录屏，导出 MP4 / GIF；录制完成后可在时间轴上选取片段，只保留需要的部分
- **截图与标注** —— 智能窗口识别与区域框选，内置标注工具，支持复制与保存
- **时间轴与关键帧** —— `analyze` 直接给出时长、分辨率、帧率、场景切换点与分段；可顺手抽若干关键帧或生成拼图。视频看流程与节奏，关键帧看 UI 细节，比只丢一段视频给模型更准、更省 token
- **面向自动化的接口** —— 无头 CLI 模式，stdout 单行 JSON 契约，`--stop-file` 随时中止，`--window` 直接按窗口标题录制，`--fps` / `--scale` / `--crf` 控制体积与清晰度，`help --json` 输出机器可读能力声明（见 [SKILL.md](SKILL.md)）
- **本地优先** —— 无账号、无遥测，默认零网络请求；录制、截图与日志全部留在本机（唯一例外是主动开启「字幕识别」时的音频转写，见下）

## 典型场景

| 场景 | light-capture 的作用 |
| --- | --- |
| 软件测试 | Agent 操作界面后回看录像，逐步核对点击、跳转与显示是否符合预期 |
| 自动剪辑 | Agent 先回看素材，理解节奏与内容后再进行剪辑 |
| 流程复盘 | 自动化流程执行完毕，以视频为依据审查效果、定位问题 |
| 演示制作 | Agent 录制操作过程，直接产出可交付的演示视频 |

## 接入说明

- **无头 CLI**：不创建窗口、不注册全局热键、零常驻，进程退出即释放全部资源，可与图形界面并存
- **stdout 契约**：成功 `{"ok":true,...}`，失败 `{"ok":false,"error":...}`，退出码 0/1，过程日志只写入本地文件
- **随时中止**：`--stop-file` 指定的文件一出现录制立即结束（百毫秒级响应），停止后自动清理
- **按窗口录制**：`light-capture list-windows` 列出当前窗口，`record --window "<标题>"` 直接录该窗口，Agent 不必自己推算坐标
- **体积可控**：`--fps 15 --scale 1280 --crf 28` 通常能把文件压到默认设置的三四成，而 UI 复盘依然看得清
- **时间轴给 Agent**：`light-capture analyze --in demo.mp4 --frames 6 --sheet sheet.jpg` 一次返回时长、分辨率、场景切换点、分段与关键帧路径
- **环境自检** —— `light-capture doctor` 明确告诉你 FFmpeg 是否可用、版本、实际解析到的路径与来源；缺失时直接给出可复制的安装命令（本项目**不打包** FFmpeg：截图不需要它，其余功能自备即可，安装包体积和许可证都保持干净）
- **能力发现**：`light-capture help --json` 输出机器可读的能力声明；完整的触发与调用指引见 [SKILL.md](SKILL.md)
- 截图同样可用：`light-capture screenshot --out shot.png [--region x,y,w,h]`
- **可定制**：快捷键、音频增益、保存目录均可在设置页修改；日志目录可用环境变量 `LIGHT_CAPTURE_LOG_DIR` 指到其它磁盘

## 人也能用

同一应用附带图形界面：托盘图标 + 快捷键唤醒，智能框选截图、区域录屏、时间轴选段与标注导出。注意 GUI 常驻托盘是为了承接用户快捷键，CLI 模式则完全不驻留。

<p align="center">
  <b>智能框选</b><br/>
  <img src="docs/images/screenshot-select.png" alt="智能框选" width="86%" />
</p>

<p align="center">
  <b>截图标注</b><br/>
  <img src="docs/images/screenshot-annotate.png" alt="截图标注" width="86%" />
</p>

<p align="center">
  <b>录后选段</b><br/>
  <img src="docs/images/screenshot-recbar.png" alt="录后选段" width="86%" />
</p>

<p align="center">
  <b>设置：快捷键、音频、指针可视化、字幕识别、FFmpeg 自检</b><br/>
  <img src="docs/images/screenshot-settings.png" alt="设置" width="72%" />
</p>

## 隐私

默认不联网：无账号、无遥测，录制与截图文件不会离开本机。

唯一例外是**可选**的「字幕识别」：它默认关闭，只有你填入自己的 API Key 并主动开启后，导出视频的音轨才会被上传到你所选的服务商（阿里云 DashScope 或火山引擎）做语音转写。关闭该功能后不会有任何网络请求。

日志与设置只写入本机。设置文件（API Key 以明文保存）位于 `%LOCALAPPDATA%\io.github.e3chovivi.light-capture\settings.json`，请自行妥善保管。

## 从源码构建

<details>
<summary>构建步骤</summary>

- Windows 10 及以上，安装 [pnpm](https://pnpm.io/)、[Rust](https://rustup.rs/)、[FFmpeg](https://ffmpeg.org/)（加入 PATH）

> FFmpeg 只影响录制/导出/抽帧/场景检测，截图不需要它。可用 `LIGHT_CAPTURE_FFMPEG` 指定自定义路径，装好后跑 `light-capture doctor` 复核。

```bash
pnpm install
pnpm tauri dev        # 开发调试
pnpm tauri build      # 打包
cd src-tauri && cargo test   # 102 个单元测试
```

</details>

## 许可证

[MIT](LICENSE)
