# light-capture 冒烟测试（Windows / PowerShell）
#
# 用法（在仓库根目录执行）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\smoke-test.ps1
#
# 它做四件事：环境自检 → 截一张图 → 录 3 秒屏 → 分析这段录像并抽帧出拼图。
# 每步打印 PASS / FAIL，最后汇总。不依赖任何第三方模块。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$exe = Join-Path $root "src-tauri\target\release\light-capture.exe"

if (-not (Test-Path $exe)) {
    $debug = Join-Path $root "src-tauri\target\debug\light-capture.exe"
    if (Test-Path $debug) { $exe = $debug } else {
        Write-Host "[FAIL] 找不到 light-capture.exe，请先构建：" -ForegroundColor Red
        Write-Host "       cd src-tauri; cargo build --release"
        exit 1
    }
}
Write-Host "被测程序: $exe`n" -ForegroundColor Cyan

$out = Join-Path $env:TEMP "lc-smoke"
New-Item -ItemType Directory -Force -Path $out | Out-Null
$results = @()

function Add-Result($name, $ok, $detail) {
    $script:results += [pscustomobject]@{ 项目 = $name; 结果 = $(if ($ok) { "PASS" } else { "FAIL" }); 说明 = $detail }
    $color = if ($ok) { "Green" } else { "Red" }
    Write-Host ("[{0}] {1}  {2}" -f $(if ($ok) { "PASS" } else { "FAIL" }), $name, $detail) -ForegroundColor $color
}

# ---------- 1) 环境自检 ----------
Write-Host "`n=== 1/4 环境自检 (doctor) ===" -ForegroundColor Cyan
$raw = & $exe doctor 2>$null
$doc = $null
$ff = $null
try { $doc = $raw | ConvertFrom-Json } catch { }
if ($null -ne $doc) {
    $ff = $doc.ffmpeg
    Add-Result "doctor 返回合法 JSON" $true "ok=$($doc.ok)"
    Add-Result "FFmpeg 可用" ([bool]$ff.found) $(if ($ff.found) { "$($ff.version)  来源=$($ff.origin)" } else { "未找到，见下方提示" })
    if (-not $ff.found) {
        Write-Host "`n安装提示：`n$($ff.hint)`n" -ForegroundColor Yellow
    }
} else {
    Add-Result "doctor 返回合法 JSON" $false "输出无法解析：$raw"
    $results | Format-Table -AutoSize
    Write-Host "`n程序没有返回可解析的 JSON，后续测试无意义，先排查这个。" -ForegroundColor Red
    exit 3
}

if (-not $ff -or -not $ff.found) {
    Write-Host "FFmpeg 不可用，跳过后续需要 FFmpeg 的测试。" -ForegroundColor Yellow
    $results | Format-Table -AutoSize
    exit 2
}

# ---------- 2) 截图 ----------
Write-Host "`n=== 2/4 截图 ===" -ForegroundColor Cyan
$shot = Join-Path $out "shot.png"
if (Test-Path $shot) { Remove-Item $shot -Force }
$raw = & $exe screenshot --out $shot 2>$null
$j = $null; try { $j = $raw | ConvertFrom-Json } catch { }
Add-Result "截图返回 JSON" ($null -ne $j -and $j.ok) $raw
Add-Result "截图文件已生成" (Test-Path $shot) $(if (Test-Path $shot) { "$([math]::Round((Get-Item $shot).Length/1KB)) KB  $shot" } else { "文件不存在" })

# ---------- 3) 录屏（含指针与微光） ----------
Write-Host "`n=== 3/4 录屏 3 秒（指针 + 微光）===" -ForegroundColor Cyan
$mp4 = Join-Path $out "demo.mp4"
if (Test-Path $mp4) { Remove-Item $mp4 -Force }
$sw = [Diagnostics.Stopwatch]::StartNew()
$raw = & $exe record --seconds 3 --out $mp4 2>$null
$sw.Stop()
$j = $null; try { $j = $raw | ConvertFrom-Json } catch { }
Add-Result "录屏返回 JSON" ($null -ne $j -and $j.ok) $raw
Add-Result "录像文件已生成" (Test-Path $mp4) $(if (Test-Path $mp4) { "$([math]::Round((Get-Item $mp4).Length/1KB)) KB  耗时 $([math]::Round($sw.Elapsed.TotalSeconds,1))s" } else { "文件不存在" })
if ($null -ne $j -and $j.ok) {
    Add-Result "返回 cursor=true" ($j.cursor -eq $true) "cursor=$($j.cursor) glow=$($j.cursor_glow) fps=$($j.fps)"
    Add-Result "时长大致为 3 秒" ([math]::Abs($j.seconds - 3) -lt 1.0) "seconds=$($j.seconds)"
}

# ---------- 4) 分析（时间轴 + 关键帧 + 拼图） ----------
Write-Host "`n=== 4/4 分析：时间轴 / 关键帧 / 拼图 ===" -ForegroundColor Cyan
$sheet = Join-Path $out "sheet.jpg"
$framesDir = Join-Path $out "frames"
if (Test-Path $sheet) { Remove-Item $sheet -Force }
if (Test-Path $framesDir) { Remove-Item $framesDir -Recurse -Force }
$raw = & $exe analyze --in $mp4 --frames 4 --frames-dir $framesDir --sheet $sheet --cols 2 2>$null
$j = $null; try { $j = $raw | ConvertFrom-Json } catch { }
if ($null -ne $j -and $j.ok) {
    $t = $j.timeline
    Add-Result "analyze 返回时间轴" $true "duration=$($t.duration)s  $($t.width)x$($t.height)  fps=$($t.fps)"
    Add-Result "有场景分段" ($t.segments.Count -ge 1) "segments=$($t.segments.Count)"
    $n = @(Get-ChildItem $framesDir -Filter *.jpg -ErrorAction SilentlyContinue).Count
    Add-Result "关键帧已生成" ($n -eq 4) "实际 $n 张"
    Add-Result "拼图已生成" (Test-Path $sheet) $sheet
} else {
    Add-Result "analyze 返回时间轴" $false $raw
}

# ---------- 汇总 ----------
Write-Host "`n=== 汇总 ===" -ForegroundColor Cyan
$results | Format-Table -AutoSize
$failed = @($results | Where-Object { $_.结果 -eq "FAIL" }).Count
Write-Host "`n输出目录: $out" -ForegroundColor Cyan
if ($failed -eq 0) {
    Write-Host "全部通过。" -ForegroundColor Green
    Write-Host "`n还需人工看两件事：" -ForegroundColor Yellow
    Write-Host "  1. 打开 $mp4 —— 画面里有没有鼠标指针？指针周围有没有贴合形状的蓝色微光？"
    Write-Host "  2. 打开 $sheet —— 拼图是否按时间顺序排列、画面是否正常。"
    exit 0
} else {
    Write-Host "有 $failed 项失败，请把上面的 FAIL 行连同日志一起发我。" -ForegroundColor Red
    Write-Host "日志: $env:LOCALAPPDATA\io.github.e3chovivi.light-capture\logs\light-capture.log"
    exit 1
}
