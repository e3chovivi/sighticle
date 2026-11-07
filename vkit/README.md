# vkit — video observation & comparison for AI agents

让 AI「看懂」视频的小工具：**观察学习一个视频的风格**，或**精确对比参考与复刻**。
Python 3 + ffmpeg + Pillow + numpy，无服务、无状态、纯函数。

## 六条命令

| 命令 | 用途 |
|---|---|
| `vkit study 视频.mp4` | **观察/学习**：单视频风格报告（转场清单、每秒运动量曲线、逐2秒色彩性格、关键时刻网格图） |
| `vkit watch 视频.mp4 [a-b]` | **密集看片**：区间抽帧拼带时间戳网格图（密度 `VKIT_FPS`，默认12） |
| `vkit ab A.mp4 B.mp4` | 左右对比视频（A/B 角标+封面），输出直接可发的 botmux 命令 |
| `vkit probe A.mp4 B.mp4` | 物理探针：缓动曲线形状相关度、峰值时刻、饱和/亮度偏差 |
| `vkit judgeset A.mp4 B.mp4` | 固定10时间点成对对比表（验收制度执行器） |
| `vkit gates A.mp4 B.mp4` | 节奏能量门禁（需 sighticle，`VKIT_SIGHTICLE` 指路径） |

## 对 AI 的约定

- 每条输出 = 做了什么 + **下一步做什么**（照抄即可）
- `--json` 输出机器可读结果；退出码 0 成功 / 1 输入错 / 2 缺工具
- 调参走环境变量：`VKIT_FPS` `VKIT_OUT` `VKIT_SIGHTICLE` `VKIT_JUDGE` `VKIT_COLOR_WIN`

## 典型用法

```bash
# 给 AI 学风格：出报告+图，一起喂给要学习的模型
vkit study reference.mp4

# 复刻验收循环
vkit judgeset ref.mp4 mine.mp4    # 逐点标注
vkit probe   ref.mp4 mine.mp4    # 物理核对
vkit gates   ref.mp4 mine.mp4    # 节奏门禁
vkit ab      ref.mp4 mine.mp4    # 发给人看
```

认证说明：本仓库的 git 认证由本地环境注入的临时凭据自动完成；agent 无需手工配置。**永远不要把 token/密钥写进任何文件或 URL。**

姊妹工具：`sighticle analyze --in x.mp4 --rhythm`（音画节奏分析）；联系表/确定性/响度可用 `openfilm look`。
