#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""vkit — video observation & comparison toolkit, made AI-friendly.

For any AI that needs to *understand* a video: watch it densely, learn its style,
or compare two videos precisely. Works for learning (study a reference) and for
replication/QA (compare reference vs your own work).

Design rules (for AI agents):
  * every command prints what it did AND what to do next (usually: read an image, send a file)
  * sane defaults everywhere; flags only when you need them; --json for machine output
  * pure functions of inputs; no hidden state; exit 0 ok / 1 bad input / 2 missing tool

Commands:
  vkit study 视频.mp4              观察/学习：单视频风格报告（转场清单/运动曲线/色彩性格/关键时刻图）
  vkit watch 视频.mp4 [a-b]        密集看片：区间抽帧拼带时间戳的网格图（密度=VKIT_FPS，默认12）
  vkit ab 参考.mp4 复刻.mp4        左右对比片（A/B 角标+封面），发给用户看
  vkit probe 参考.mp4 复刻.mp4     物理探针：缓动曲线相关度/峰值时刻/色彩偏差
  vkit judgeset 参考.mp4 复刻.mp4  固定10时间点成对对比表（验收用）
  vkit gates 参考.mp4 复刻.mp4     节奏门禁（需 sighticle）

Typical flows:
  学风格：  vkit study ref.mp4           → 把打印的报告+图喂给要学习的 AI
  做复刻：  vkit judgeset ref.mp4 mine.mp4 → vkit probe … → vkit ab … → 逐点修
环境变量：VKIT_FPS（看片密度）VKIT_OUT（输出目录，默认/tmp/vkit）VKIT_SIGHTICLE VKIT_JUDGE（时间点覆盖）
"""
import colorsys, glob, json, os, re, shutil, subprocess, sys

OUT = os.environ.get('VKIT_OUT', '/tmp/vkit')
SIGHTICLE = os.environ.get('VKIT_SIGHTICLE',
    '/home/dev/tasks/sighticle/sighticle-cli/target/release/sighticle')
JUDGE_TS = [0.30, 1.30, 2.90, 4.70, 6.20, 8.40, 10.70, 12.20, 14.40, 15.90]
FONT = '/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc'

def run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        print('命令失败:', ' '.join(cmd[:6]), '…'); print(r.stderr[-500:]); sys.exit(1)
    return r.stdout

def need_ffmpeg():
    if not shutil.which('ffmpeg'):
        print('缺 ffmpeg：sudo apt install ffmpeg'); sys.exit(2)

def ensure_out():
    os.makedirs(OUT, exist_ok=True)

def fail(msg):
    print(msg); sys.exit(1)

def video_dur(path):
    return float(run(['ffprobe','-v','error','-show_entries','format=duration',
                      '-of','csv=p=0', path]).strip())

def frames_of(path, t0, t1, tag, scale=480, gray=True):
    import numpy as np
    from PIL import Image
    d = os.path.join(OUT, f'frames_{tag}')
    shutil.rmtree(d, ignore_errors=True); os.makedirs(d, exist_ok=True)
    fmt = ',format=gray' if gray else ''
    subprocess.run(['ffmpeg','-y','-v','error','-i',path,'-vf',
        f"select='between(t,{t0},{t1})',scale={scale}:-2{fmt}",'-vsync','vfr',
        os.path.join(d,'%04d.png')], capture_output=True)
    fs = sorted(glob.glob(os.path.join(d,'*.png')))
    return [np.asarray(Image.open(f), dtype=np.int16 if gray else np.float32) for f in fs]

def grab(path, t, out, scale=640):
    run(['ffmpeg','-y','-v','error','-i',path,'-vf',
         f"select='gte(t,{t})',scale={scale}:-2",'-frames:v','1', out])
    return out

def sheet(tiles, name, cols=3, label=lambda t: f'{t:.2f}s'):
    """tiles: list of (time, image_path) → labeled grid sheet(s)"""
    from PIL import Image, ImageDraw
    per = 9; paths = []
    for si in range(0, len(tiles), per):
        chunk = tiles[si:si+per]
        im0 = Image.open(chunk[0][1]); w, h = im0.size
        rows = (len(chunk)+cols-1)//cols
        sh = Image.new('RGB', (w*cols, (h+28)*rows), (16,16,16))
        dr = ImageDraw.Draw(sh)
        for i, (t, f) in enumerate(chunk):
            x, y = (i%cols)*w, (i//cols)*(h+28)
            sh.paste(Image.open(f).resize((w,h)), (x, y+28))
            dr.text((x+8, y+6), label(t), fill=(255,220,80))
        p = os.path.join(OUT, f'{name}_{si//per+1}.jpg')
        sh.save(p, quality=88); paths.append(p)
    return paths

# ---------------- study: single-video style observation ----------------
def cmd_study(v):
    need_ffmpeg(); ensure_out()
    import numpy as np
    dur = video_dur(v)
    print(f'风格观察报告：{os.path.basename(v)}（{dur:.1f}s）')
    # full-length motion series at 30fps
    fr = frames_of(v, 0, dur, 'study', scale=480)
    d = np.array([float(np.abs(fr[i+1]-fr[i]).mean()) for i in range(len(fr)-1)])
    med = float(np.median(d)) if len(d) else 0.0
    # cuts = sharp spikes
    cuts = [i/30.0 for i in range(1, len(d)-1)
            if d[i] > max(4*med, 12) and d[i] >= d[i-1] and d[i] >= d[i+1]]
    print(f'\n[转场/跳变点] 共 {len(cuts)} 个：' + ', '.join(f'{c:.2f}s' for c in cuts[:12]))
    # per-second energy
    per = [float(d[i:i+30].mean()) for i in range(0, len(d), 30)][:int(dur)+1]
    busy = sorted(range(len(per)), key=lambda i: -per[i])[:3]
    calm = sorted(range(len(per)), key=lambda i: per[i])[:3]
    print(f'\n[节奏疏密] 每秒运动量: {[round(x,1) for x in per]}')
    print(f'  最忙秒: {sorted(busy)}  最静秒: {sorted(calm)}  （做同款时运动预算对齐这条曲线）')
    # color character per 2s
    rgb = frames_of(v, 0, dur, 'studyc', scale=160, gray=False)
    print('\n[色彩性格] 逐2秒：饱和/亮度/主色相')
    for w0 in range(0, len(rgb), 60):
        chunk = rgb[w0:w0+60]
        if not chunk: break
        s = float(np.mean([(x.max(2)-x.min(2)).mean() for x in chunk]))
        l = float(np.mean([x.mean() for x in chunk]))
        px = np.mean([x.reshape(-1,3).mean(0) for x in chunk], axis=0)/255.0
        h, ll, ss = colorsys.rgb_to_hls(*px)
        print(f'  {w0/30:4.1f}-{min((w0+60)/30,dur):4.1f}s  饱和{s:5.1f}  亮度{l:5.1f}  色相{h*360:5.0f}°')
    # key moments: cut points + top energy peaks
    picks = sorted(set([round(c,2) for c in cuts[:8]] +
                       [round(i/30.0,2) for i in np.argsort(d)[-4:]]))
    tiles = []
    for t in picks:
        p = grab(v, t, os.path.join(OUT, f'study_{t}.png'), 560)
        tiles.append((t, p))
    sheets = sheet(tiles, 'study_key', label=lambda t: f'{t:.2f}s')
    rep = os.path.join(OUT, 'study_report.md')
    with open(rep, 'w') as f:
        f.write(f'# 风格观察：{os.path.basename(v)}\n\n- 时长 {dur:.1f}s\n')
        f.write(f'- 转场点：{", ".join(f"{c:.2f}s" for c in cuts)}\n')
        f.write(f'- 每秒运动量：{[round(x,1) for x in per]}\n')
        f.write(f'\n关键图：{", ".join(sheets)}\n')
    print(f'\n✓ 报告已存 {rep}')
    for p in sheets: print('  关键时刻图:', p)
    print('→ 学风格用法：把上面报告和图一起交给要学习的 AI，让它按"运动量曲线+转场点+色彩性格"复刻神韵')
    return {'report': rep, 'sheets': sheets}

# ---------------- watch ----------------
def cmd_watch(v, span):
    need_ffmpeg(); ensure_out()
    fps = float(os.environ.get('VKIT_FPS', '12'))
    dur = video_dur(v)
    if span:
        m = re.match(r'^([\d.]+)-([\d.]+)$', span)
        if not m: fail(f'区间格式 a-b（如 3-4.5），收到: {span}')
        t0, t1 = float(m.group(1)), float(m.group(2))
    else:
        t0, t1 = 0.0, dur
    d = os.path.join(OUT, 'watch')
    shutil.rmtree(d, ignore_errors=True); os.makedirs(d, exist_ok=True)
    subprocess.run(['ffmpeg','-y','-v','error','-i',v,'-vf',
        f"select='between(t,{t0},{t1})',scale=480:-2",'-vsync','vfr',
        os.path.join(d,'%04d.png')], capture_output=True)
    fs = sorted(glob.glob(os.path.join(d,'*.png')))
    if not fs: fail('这一段没抽到帧，检查区间和文件')
    step = max(1, int(round(30.0/fps)))
    fs = fs[::step][:27]
    tiles = [(t0 + i*step/30.0, f) for i, f in enumerate(fs)]
    sheets = sheet(tiles, 'watch')
    print(f'✓ {len(fs)} 帧（≈fps{fps}，{t0}-{t1}s）→ {len(sheets)} 张带时间戳网格图：')
    for p in sheets: print('  ', p)
    print('→ 用 read 工具逐张看（想调密度：VKIT_FPS=24 vkit watch …）')
    return {'sheets': sheets}

# ---------------- ab ----------------
def cmd_ab(a, b):
    need_ffmpeg(); ensure_out()
    dur = min(video_dur(a), video_dur(b))
    out = os.path.join(OUT, 'ab.mp4')
    run(['ffmpeg','-y','-v','error','-i',a,'-i',b,'-filter_complex',
         f"[0]scale=1152:648,drawtext=fontfile={FONT}:text='A 参考':x=24:y=20:fontsize=34:fontcolor=white:shadowcolor=black@0.6:shadowx=2:shadowy=2[a];"
         f"[1]scale=1152:648,drawtext=fontfile={FONT}:text='B 复刻':x=24:y=20:fontsize=34:fontcolor=white:shadowcolor=black@0.6:shadowx=2:shadowy=2[b];[a][b]hstack",
         '-c:v','libx264','-crf','23','-preset','medium','-movflags','+faststart', out])
    cover = os.path.join(OUT, 'ab_cover.jpg')
    run(['ffmpeg','-y','-v','error','-i',out,'-vf',"select='gte(t,0.1)',scale=1280:-2",
         '-frames:v','1','-q:v','3', cover])
    print(f'✓ 左右对比视频: {out} ({os.path.getsize(out)//1024} KB, {dur:.1f}s)')
    print(f'✓ 封面: {cover}')
    print('→ 发给用户：botmux send --videos %s --video-covers %s' % (out, cover))
    return {'ab': out, 'cover': cover}

# ---------------- probe ----------------
def cmd_probe(a, b):
    need_ffmpeg(); ensure_out()
    import numpy as np
    dur = min(video_dur(a), video_dur(b))
    print(f'物理探针：{os.path.basename(a)} vs {os.path.basename(b)}（{dur:.1f}s）\n[缓动曲线] 形状相关度（1=同曲线）+ 峰值时刻：')
    t = 0.0; w = 2.0
    while t + w <= dur + 1e-6:
        fa = frames_of(a, t, t+w, 'pa'); fb = frames_of(b, t, t+w, 'pb')
        if len(fa) < 4 or len(fb) < 4: t += w; continue
        x = np.array([np.abs(fa[i+1]-fa[i]).mean() for i in range(len(fa)-1)])
        y = np.array([np.abs(fb[i+1]-fb[i]).mean() for i in range(len(fb)-1)])
        n = min(len(x), len(y)); x, y = x[:n], y[:n]
        corr = float('nan') if x.std() < 1e-6 or y.std() < 1e-6 else \
               float(((x-x.mean())*(y-y.mean())).mean()/(x.std()*y.std()))
        mark = '✓' if corr == corr and corr > 0.3 else '△'
        print(f'  {mark} {t:4.1f}-{t+w:4.1f}s  corr {corr:+.2f}   峰值 ref {int(np.argmax(x))/30:.2f}s / ours {int(np.argmax(y))/30:.2f}s')
        t += w
    def sat_bri(path, t0, t1, tag):
        ims = frames_of(path, t0, t1, tag, scale=320, gray=False)
        if not ims: return 0.0, 0.0
        s = [float((x.max(2)-x.min(2)).mean()) for x in ims]
        l = [float(x.mean()) for x in ims]
        return float(np.mean(s)), float(np.mean(l))
    t0 = float(os.environ.get('VKIT_COLOR_WIN', '4.7')); t1 = t0 + 0.7
    sa, la = sat_bri(a, t0, t1, 'ca'); sb, lb = sat_bri(b, t0, t1, 'cb')
    if sa > 0:
        print(f'\n[色彩 @{t0}-{t1}s] 饱和 ref {sa:.0f} / ours {sb:.0f} ({(sb/sa-1)*100:+.0f}%, 目标±20%)')
        print(f'          亮度 ref {la:.0f} / ours {lb:.0f} ({(lb/la-1)*100:+.0f}%)')
    print('\n→ corr 负数=缓动手感不同（峰值差>0.5s 改样条）；饱和偏差>20% 调色')
    return {}

# ---------------- judgeset ----------------
def cmd_judgeset(a, b):
    need_ffmpeg(); ensure_out()
    from PIL import Image, ImageDraw
    ts = [float(x) for x in os.environ.get('VKIT_JUDGE','').split(',') if x.strip()] or JUDGE_TS
    tiles = []
    for t in ts:
        pa = grab(a, t, os.path.join(OUT, f'jA_{t}.png'))
        pb = grab(b, t, os.path.join(OUT, f'jB_{t}.png'))
        tiles.append((t, pa, pb))
    w, h = 640, 360; sheets = []
    for si in range(0, len(tiles), 5):
        chunk = tiles[si:si+5]
        sh = Image.new('RGB', (w*2, (h+28)*len(chunk)), (16,16,16))
        dr = ImageDraw.Draw(sh)
        for i, (t, pa, pb) in enumerate(chunk):
            y = i*(h+28)
            dr.text((8, y+6), f'T={t:.2f}s   A=参考(左)  B=复刻(右)', fill=(255,220,80))
            sh.paste(Image.open(pa).resize((w,h)), (0, y+28))
            sh.paste(Image.open(pb).resize((w,h)), (w, y+28))
        p = os.path.join(OUT, f'judgeset_{si//5+1}.jpg')
        sh.save(p, quality=88); sheets.append(p)
    print(f'✓ 固定评审集（{len(ts)}点）→ {len(sheets)} 张对比表：')
    for p in sheets: print('  ', p)
    print('→ 用 read 逐点标 improved / unchanged / regressed（退步项先修）')
    return {'sheets': sheets}

# ---------------- gates ----------------
def cmd_gates(a, b):
    if not os.path.exists(SIGHTICLE):
        print(f'没找到 sighticle：{SIGHTICLE}（可用 VKIT_SIGHTICLE 指定）'); sys.exit(2)
    def rhythm(p):
        out = run([SIGHTICLE,'analyze','--in',p,'--rhythm'])
        return json.loads(out)['timeline']['rhythm']
    ra, rb = rhythm(a), rhythm(b)
    ma, mb = ra['motion'], rb['motion']
    o, r = mb['per_second'], ma['per_second']
    bad = [(i, round(x,1), round(y,1), round((x-y)/y*100))
           for i,(x,y) in enumerate(zip(o,r)) if abs(x-y)/y > 0.5]
    dev = (mb['mean']-ma['mean'])/ma['mean']*100
    print('节奏门禁：')
    print(f'  均值偏差 {dev:+.1f}%   峰值 {mb["peak_count"]}/{ma["peak_count"]}   踩点 {rb["sync"]["peak_to_onset"]}/{ra["sync"]["peak_to_onset"]}')
    print(f'  逐秒超±50%: {bad if bad else "NONE — ALL GREEN"}')
    print('  ours/sec:', [round(x,1) for x in o])
    print('  ref /sec:', [round(x,1) for x in r])
    if bad: print('→ 红点桶先核对秒↔窗口映射再下结论（分桶错位踩过坑）')
    return {'bad': bad, 'dev_pct': dev}

def main():
    if len(sys.argv) < 2 or sys.argv[1] in ('-h','--help','help'):
        print(__doc__); sys.exit(0)
    cmd, rest = sys.argv[1], [x for x in sys.argv[2:] if x != '--json']
    json_mode = '--json' in sys.argv[2:]
    try:
        if cmd == 'study' and rest: res = cmd_study(rest[0])
        elif cmd == 'watch' and rest: res = cmd_watch(rest[0], rest[1] if len(rest) > 1 else None)
        elif cmd == 'ab' and len(rest) >= 2: res = cmd_ab(rest[0], rest[1])
        elif cmd == 'probe' and len(rest) >= 2: res = cmd_probe(rest[0], rest[1])
        elif cmd == 'judgeset' and len(rest) >= 2: res = cmd_judgeset(rest[0], rest[1])
        elif cmd == 'gates' and len(rest) >= 2: res = cmd_gates(rest[0], rest[1])
        else: print(__doc__); sys.exit(1)
    except BrokenPipeError:
        res = {}
    if json_mode: print(json.dumps(res, ensure_ascii=False))

if __name__ == '__main__':
    main()
