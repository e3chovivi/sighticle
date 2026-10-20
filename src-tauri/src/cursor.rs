//! 指针可视化：把真实系统指针原样画进录制画面，并沿其图标轮廓叠加蓝色微光。
//!
//! 为什么要自己画：xcap 0.9.8 的 WGC 录制里固定调用了
//! `SetIsCursorCaptureEnabled(false)`，采集到的画面里没有指针。而对「让 AI 回看
//! 交互过程」这个用途来说，"点在哪里、拖了什么路径"恰恰是最关键的信息。
//!
//! 指针图标本身原样保留（箭头就是箭头、I 型就是 I 型、手型就是手型），
//! 不做任何替换或简化——AI 与人都靠图标形状判断当前可做什么操作。
//!
//! 微光不是圆心渐变的圆形光斑，而是**按图标自身的轮廓**生成的：
//! 先对指针的 alpha 掩码做距离变换，得到每个像素到图标边缘的距离，
//! 再按距离衰减上色。于是斜箭头是斜的光晕、I 型光标是细长的一条，
//! 光始终贴着图标本身的符号形状走，不会在指针旁边多出一个"圆球"。
//!
//! 性能：位置每帧查询一次，但贴图与光晕按（句柄+尺寸+开关）缓存，形状不变时
//! 只做一次局部混合。另设 150ms 的贴图刷新周期——动画指针（忙等转圈）的句柄
//! 始终不变，只按句柄缓存会把转圈冻结在某一帧；重建成本很小，8Hz 足够还原动画。

// 纯几何/渐变函数在非 Windows 上只被测试调用，不该报 dead_code
#![cfg_attr(not(windows), allow(dead_code))]

/// 光晕主色（蓝）
const GLOW_RGB: [u8; 3] = [80, 170, 255];
/// 光晕峰值不透明度（紧贴轮廓处）
const GLOW_PEAK: f32 = 0.85;
/// 光晕最小半径（物理像素）
const GLOW_RADIUS_MIN: f32 = 10.0;
/// 光晕半径相对指针宽度的比例。取小值才有"沿轮廓描边"的观感；
/// 取大值会糊成一块蓝色贴片，反而盖住背景内容。
const GLOW_RADIUS_RATIO: f32 = 0.35;
/// 光晕衰减曲线指数：越大收得越快、越锐利
const GLOW_FALLOFF: f32 = 2.5;
/// 判定"这里是图标本体"的 alpha 阈值（过滤抗锯齿产生的极淡边缘）
const GLOW_MASK_THRESHOLD: u8 = 24;
/// 贴图最长复用时间。动画指针的句柄不变，必须定期重绘才能还原动画。
#[cfg(windows)]
const SPRITE_TTL: std::time::Duration = std::time::Duration::from_millis(150);

/// 按指针尺寸决定光晕半径：指针越大，光晕也要跟着放大，
/// 否则在 200% 缩放的屏幕上会缩成一小圈。
fn glow_radius(icon_w: u32) -> f32 {
    GLOW_RADIUS_MIN.max(icon_w as f32 * GLOW_RADIUS_RATIO)
}

/// 屏幕上的一个矩形（物理像素）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// 与帧范围求交，返回帧内可写区域；完全在帧外返回 None。
    /// `fw`/`fh` 是帧尺寸，帧内坐标原点为 (0,0)。
    pub fn clip(&self, fw: u32, fh: u32) -> Option<(Rect, u32, u32)> {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = (self.x + self.w as i32).min(fw as i32);
        let y1 = (self.y + self.h as i32).min(fh as i32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some((
            Rect {
                x: x0,
                y: y0,
                w: (x1 - x0) as u32,
                h: (y1 - y0) as u32,
            },
            (x0 - self.x) as u32,
            (y0 - self.y) as u32,
        ))
    }
}

/// 光晕在「距图标轮廓 `d` 像素」处的不透明度（0~1）。
/// 紧贴轮廓处最亮，向外按 `GLOW_FALLOFF` 次幂快速收掉——这样光晕才有
/// "沿符号描边"的观感，而不是一个与图标形状无关的圆形光斑。
fn silhouette_alpha(d: f32, radius: f32) -> f32 {
    if d < 0.0 || radius <= 0.0 || d >= radius {
        return 0.0;
    }
    let x = d / radius;
    GLOW_PEAK * (1.0 - x).powf(GLOW_FALLOFF)
}

/// 两遍 chamfer 距离变换：求每个像素到最近"图标本体"像素的近似欧氏距离。
/// 返回的掩码尺寸为 `w * h`，0 表示本像素属于图标本体。
fn distance_transform(mask: &[u8], w: u32, h: u32) -> Vec<f32> {
    let w = w as usize;
    let h = h as usize;
    let mut d = vec![f32::MAX; w * h];
    for i in 0..w * h {
        if mask[i] >= GLOW_MASK_THRESHOLD {
            d[i] = 0.0;
        }
    }
    const DIAG: f32 = std::f32::consts::SQRT_2;
    // 正向：左上 → 右下
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let mut best = d[i];
            if x > 0 {
                best = best.min(d[i - 1] + 1.0);
            }
            if y > 0 {
                best = best.min(d[i - w] + 1.0);
                if x > 0 {
                    best = best.min(d[i - w - 1] + DIAG);
                }
                if x + 1 < w {
                    best = best.min(d[i - w + 1] + DIAG);
                }
            }
            d[i] = best;
        }
    }
    // 反向：右下 → 左上
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let i = y * w + x;
            let mut best = d[i];
            if x + 1 < w {
                best = best.min(d[i + 1] + 1.0);
            }
            if y + 1 < h {
                best = best.min(d[i + w] + 1.0);
                if x + 1 < w {
                    best = best.min(d[i + w + 1] + DIAG);
                }
                if x > 0 {
                    best = best.min(d[i + w - 1] + DIAG);
                }
            }
            d[i] = best;
        }
    }
    d
}

/// 按指针图标的轮廓生成蓝色微光层（RGBA，尺寸为图标四周各留 `pad`）。
/// `cursor` 是图标本体的 RGBA 数据，尺寸 `cw * ch`。
fn build_glow_layer(cursor: &[u8], cw: u32, ch: u32, pad: u32, radius: f32) -> Vec<u8> {
    let w = cw + pad * 2;
    let h = ch + pad * 2;
    // 把图标 alpha 放进带留白的掩码里，再对整块做距离变换
    let mut mask = vec![0u8; (w * h) as usize];
    for y in 0..ch {
        for x in 0..cw {
            let a = cursor[((y * cw + x) * 4 + 3) as usize];
            mask[((y + pad) * w + x + pad) as usize] = a;
        }
    }
    let dist = distance_transform(&mask, w, h);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        let a = silhouette_alpha(dist[i], radius);
        if a <= 0.0 {
            continue;
        }
        rgba[i * 4] = GLOW_RGB[0];
        rgba[i * 4 + 1] = GLOW_RGB[1];
        rgba[i * 4 + 2] = GLOW_RGB[2];
        rgba[i * 4 + 3] = (a * 255.0) as u8;
    }
    rgba
}

/// 把 RGBA 源像素叠加到 RGB24 目标像素上。
/// 纯整数运算，避免每帧浮点混合带来的开销与平台差异。
fn blend_rgba_over_rgb(dst: &mut [u8], src: [u8; 4]) {
    let a = src[3] as u32;
    if a == 0 {
        return;
    }
    let inv = 255 - a;
    for i in 0..3 {
        dst[i] = ((src[i] as u32 * a + dst[i] as u32 * inv + 127) / 255) as u8;
    }
}

/// 指针贴图：RGBA，已包含光晕留白。
#[cfg(windows)]
struct Sprite {
    w: u32,
    h: u32,
    /// 热点在贴图内的坐标（贴图左上角为原点）
    offset_x: u32,
    offset_y: u32,
    rgba: Vec<u8>,
}

pub struct CursorCompositor {
    enabled: bool,
    glow: bool,
    #[cfg(windows)]
    sprite: Option<Sprite>,
    /// 缓存键：指针句柄 + 系统指针尺寸 + 是否带光晕
    #[cfg(windows)]
    sprite_key: Option<(usize, u32, u32, bool)>,
    /// 贴图生成时刻（用于动画指针的定期刷新）
    #[cfg(windows)]
    sprite_at: Option<std::time::Instant>,
}

/// 计算指针贴图在帧内的可见区域。
///
/// `(cx, cy)` 是指针热点的全局屏幕坐标，`(origin_x, origin_y)` 是帧左上角的全局坐标，
/// `(offset_x, offset_y)` 是热点在贴图内的位置。返回（帧内可见矩形, 贴图内起始 x, y）。
#[allow(clippy::too_many_arguments)]
fn place_sprite(
    cx: i32,
    cy: i32,
    offset_x: u32,
    offset_y: u32,
    sprite_w: u32,
    sprite_h: u32,
    fw: u32,
    fh: u32,
    origin_x: i32,
    origin_y: i32,
) -> Option<(Rect, u32, u32)> {
    Rect {
        x: cx - offset_x as i32 - origin_x,
        y: cy - offset_y as i32 - origin_y,
        w: sprite_w,
        h: sprite_h,
    }
    .clip(fw, fh)
}

impl CursorCompositor {
    pub fn new(enabled: bool, glow: bool) -> Self {
        Self {
            enabled,
            glow,
            #[cfg(windows)]
            sprite: None,
            #[cfg(windows)]
            sprite_key: None,
            #[cfg(windows)]
            sprite_at: None,
        }
    }

    /// 是否启用。关闭时调用方可以走零拷贝的直写路径。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 把指针与光晕合成进一帧 RGB24 数据（原地修改）。
    /// `origin_x`/`origin_y`：帧左上角对应的全局屏幕物理坐标。
    /// 传入的帧必须是**未合成过**的干净画面（合成结果不要回写复用）。
    /// 返回本次被改写的帧内区域；没有画任何东西时返回 None。
    pub fn composite(
        &mut self,
        frame: &mut [u8],
        fw: u32,
        fh: u32,
        origin_x: i32,
        origin_y: i32,
    ) -> Option<Rect> {
        if !self.enabled || frame.len() < (fw as usize) * (fh as usize) * 3 {
            return None;
        }
        #[cfg(windows)]
        {
            let (cx, cy, offset_x, offset_y) = cursor_position_and_offset(self)?;
            let sprite = self.sprite.as_ref()?;
            let (visible, src_x, src_y) = place_sprite(
                cx, cy, offset_x, offset_y, sprite.w, sprite.h, fw, fh, origin_x, origin_y,
            )?;
            let stride = fw as usize * 3;
            for row in 0..visible.h {
                let sy = src_y + row;
                let dy = visible.y as usize + row as usize;
                for col in 0..visible.w {
                    let sx = src_x + col;
                    let dx = visible.x as usize + col as usize;
                    let si = ((sy * sprite.w + sx) * 4) as usize;
                    let di = dy * stride + dx * 3;
                    let px = [
                        sprite.rgba[si],
                        sprite.rgba[si + 1],
                        sprite.rgba[si + 2],
                        sprite.rgba[si + 3],
                    ];
                    blend_rgba_over_rgb(&mut frame[di..di + 3], px);
                }
            }
            Some(visible)
        }
        #[cfg(not(windows))]
        {
            let _ = (fw, fh, origin_x, origin_y);
            None
        }
    }
}

/// 把 `src`（干净帧）中指定区域写回 `dst`（已合成帧），擦掉上一帧画上去的指针。
/// 只回填指针覆盖过的小矩形，避免每帧复制整幅画面——4K 下整帧拷贝约占一两个百分点 CPU。
/// 区域来自 `Rect::clip`，调用方保证落在帧内；越界时直接返回 false 不做任何写入。
pub fn restore_rect(dst: &mut [u8], src: &[u8], fw: u32, r: Rect) -> bool {
    let stride = fw as usize * 3;
    if r.x < 0 || r.y < 0 || r.x as u32 + r.w > fw {
        return false;
    }
    let need = (r.y as usize + r.h as usize) * stride;
    if dst.len() < need || src.len() < need || r.w == 0 || r.h == 0 {
        return false;
    }
    for row in 0..r.h as usize {
        let start = (r.y as usize + row) * stride + r.x as usize * 3;
        let end = start + r.w as usize * 3;
        dst[start..end].copy_from_slice(&src[start..end]);
    }
    true
}

/// 查询当前指针位置，必要时重建贴图；返回（屏幕坐标 x, y, 贴图热点 x, y）。
#[cfg(windows)]
fn cursor_position_and_offset(c: &mut CursorCompositor) -> Option<(i32, i32, u32, u32)> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorInfo, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING, SM_CXCURSOR, SM_CYCURSOR,
    };

    let mut info = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetCursorInfo(&mut info) }.ok()?;
    // 指针被隐藏（比如全屏游戏/锁屏）时不该凭空画一个
    if (info.flags.0 & CURSOR_SHOWING.0) == 0 || info.hCursor.is_invalid() {
        return None;
    }

    // 尺寸也进缓存键：指针跨到不同缩放的显示器时系统会换尺寸，
    // 否则贴图会一直是旧尺寸
    let size = unsafe {
        (
            GetSystemMetrics(SM_CXCURSOR).max(16) as u32,
            GetSystemMetrics(SM_CYCURSOR).max(16) as u32,
        )
    };
    let key = (info.hCursor.0 as usize, size.0, size.1, c.glow);
    let stale = c.sprite_key.map(|k| k != key).unwrap_or(true);
    let expired = c
        .sprite_at
        .map(|t| t.elapsed() >= SPRITE_TTL)
        .unwrap_or(true);
    if stale || expired || c.sprite.is_none() {
        c.sprite = render_sprite(info.hCursor, c.glow);
        c.sprite_key = Some(key);
        c.sprite_at = Some(std::time::Instant::now());
    }
    let sprite = c.sprite.as_ref()?;
    Some((
        info.ptScreenPos.x,
        info.ptScreenPos.y,
        sprite.offset_x,
        sprite.offset_y,
    ))
}

/// 读取指针位图的真实像素尺寸。
/// 彩色指针取 `hbmColor`；单色指针只有掩码位图，且高度是「AND 掩码 + XOR 掩码」的
/// 两倍，需要除以 2。
#[cfg(windows)]
fn cursor_bitmap_size(
    icon: &windows::Win32::UI::WindowsAndMessaging::ICONINFO,
) -> Option<(u32, u32)> {
    use windows::Win32::Graphics::Gdi::{GetObjectW, BITMAP, HGDIOBJ};

    let monochrome = icon.hbmColor.is_invalid();
    let hbm = if monochrome {
        icon.hbmMask
    } else {
        icon.hbmColor
    };
    if hbm.is_invalid() {
        return None;
    }
    let mut bm = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut core::ffi::c_void),
        )
    };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    let h = if monochrome {
        bm.bmHeight / 2
    } else {
        bm.bmHeight
    };
    Some((bm.bmWidth as u32, h.max(1) as u32))
}

/// 用 GDI 把系统指针绘成位图，再按 alpha 合成到光晕底图上。
#[cfg(windows)]
fn render_sprite(
    hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
    glow: bool,
) -> Option<Sprite> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetIconInfo, GetSystemMetrics, ICONINFO, SM_CXCURSOR, SM_CYCURSOR,
    };

    let mut icon = ICONINFO::default();
    unsafe { GetIconInfo(hcursor.into(), &mut icon) }.ok()?;
    let hotspot_x = icon.xHotspot;
    let hotspot_y = icon.yHotspot;
    // 优先用位图真实尺寸（无障碍大指针、自定义指针都靠它），拿不到再退回系统指标
    let (icon_w, icon_h) = cursor_bitmap_size(&icon).unwrap_or_else(|| unsafe {
        (
            GetSystemMetrics(SM_CXCURSOR).max(16) as u32,
            GetSystemMetrics(SM_CYCURSOR).max(16) as u32,
        )
    });
    // GetIconInfo 返回的两个位图由调用方负责释放
    if !icon.hbmMask.is_invalid() {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(
                windows::Win32::Graphics::Gdi::HGDIOBJ(icon.hbmMask.0),
            );
        }
    }
    if !icon.hbmColor.is_invalid() {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(
                windows::Win32::Graphics::Gdi::HGDIOBJ(icon.hbmColor.0),
            );
        }
    }

    // 把指针分别画到黑底和白底上，再由两幅图的差值还原每个像素的 alpha 与颜色。
    // 这样同时适配两类指针：带 alpha 通道的现代指针，以及只有掩码的黑白老式指针
    // （未绘制处会完整露出背景，差值即为满 alpha）。
    let black = draw_cursor_bgra(hcursor, icon_w, icon_h, [0, 0, 0, 255])?;
    let white = draw_cursor_bgra(hcursor, icon_w, icon_h, [255, 255, 255, 255])?;

    // 先把指针还原成独立的 RGBA 图标（形状、颜色都保持系统原样）
    let mut cursor = vec![0u8; (icon_w * icon_h * 4) as usize];
    for y in 0..icon_h {
        for x in 0..icon_w {
            let si = ((y * icon_w + x) * 4) as usize;
            let px = recover_rgba(
                [black[si], black[si + 1], black[si + 2]],
                [white[si], white[si + 1], white[si + 2]],
            );
            cursor[si..si + 4].copy_from_slice(&px);
        }
    }

    let radius = glow_radius(icon_w);
    let pad = if glow { radius.ceil() as u32 + 2 } else { 2 };
    let w = icon_w + pad * 2;
    let h = icon_h + pad * 2;
    // 热点不能超出贴图范围，否则位置会整体偏掉
    let offset_x = (pad + hotspot_x).min(w.saturating_sub(1));
    let offset_y = (pad + hotspot_y).min(h.saturating_sub(1));

    // 光晕沿图标轮廓生成；图标本体最后盖上去，保证指针颜色不被染色
    let mut rgba = if glow {
        build_glow_layer(&cursor, icon_w, icon_h, pad, radius)
    } else {
        vec![0u8; (w * h * 4) as usize]
    };
    for y in 0..icon_h {
        for x in 0..icon_w {
            let si = ((y * icon_w + x) * 4) as usize;
            let px = [cursor[si], cursor[si + 1], cursor[si + 2], cursor[si + 3]];
            if px[3] == 0 {
                continue;
            }
            let di = (((y + pad) * w + (x + pad)) * 4) as usize;
            let dst = [rgba[di], rgba[di + 1], rgba[di + 2], rgba[di + 3]];
            let out = over(px, dst);
            rgba[di..di + 4].copy_from_slice(&out);
        }
    }

    Some(Sprite {
        w,
        h,
        offset_x,
        offset_y,
        rgba,
    })
}

/// 由黑底/白底两次绘制结果还原像素的 RGBA（输入顺序为 BGR）。
///
/// 记指针原色 C、覆盖率 a：黑底上 C*a，白底上 C*a + 255*(1-a)；
/// 两者之差就是 255*(1-a)，据此得到 a，再反解 C。
fn recover_rgba(black: [u8; 3], white: [u8; 3]) -> [u8; 4] {
    let mut delta = 0u32;
    for i in 0..3 {
        delta += (white[i] as i32 - black[i] as i32).unsigned_abs();
    }
    let alpha = 255u32.saturating_sub(delta / 3);
    if alpha == 0 {
        return [0, 0, 0, 0];
    }
    let mut out = [0u8, 0, 0, alpha as u8];
    for i in 0..3 {
        out[i] = ((black[i] as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
    }
    // 还原顺序是 BGR，翻转成 RGB
    [out[2], out[1], out[0], out[3]]
}

/// source-over 合成（src 叠在 dst 上），返回不透明结果。
fn over(src: [u8; 4], dst: [u8; 4]) -> [u8; 4] {
    let sa = src[3] as u32;
    if sa == 255 || dst[3] == 0 {
        return src;
    }
    let da = dst[3] as u32;
    let inv = 255 - sa;
    let out_a = sa + da * inv / 255;
    let mut out = [0u8; 4];
    if out_a == 0 {
        return [0, 0, 0, 0];
    }
    for i in 0..3 {
        out[i] = ((src[i] as u32 * sa + dst[i] as u32 * da * inv / 255) / out_a) as u8;
    }
    out[3] = out_a as u8;
    out
}

/// 把指针绘制到 32bpp 顶朝下 DIB，返回 BGRA 字节。`fill` 是绘制前的底色。
#[cfg(windows)]
fn draw_cursor_bgra(
    hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
    w: u32,
    h: u32,
    fill: [u8; 4],
) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DrawIconEx, DI_NORMAL};

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            // 负数 = 顶朝下，行序与我们的帧一致，免去翻转
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        bmiColors: [Default::default()],
    };

    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        return None;
    }
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) };
    let bitmap = match bitmap {
        Ok(b) => b,
        Err(_) => {
            unsafe {
                let _ = DeleteDC(dc);
            }
            return None;
        }
    };
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
        }
        return None;
    }

    let old = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
    let len = (w * h * 4) as usize;
    let out = unsafe {
        let slice = std::slice::from_raw_parts_mut(bits as *mut u8, len);
        for px in slice.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&fill);
        }
        let _ = DrawIconEx(
            dc,
            0,
            0,
            hcursor.into(),
            w as i32,
            h as i32,
            0,
            None,
            DI_NORMAL,
        );
        slice.to_vec()
    };

    unsafe {
        let _ = SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(dc);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silhouette_glow_is_strong_at_edge_and_fades_outward() {
        let r = 16.0;
        assert!(
            silhouette_alpha(0.0, r) > GLOW_PEAK * 0.95,
            "紧贴轮廓应最亮"
        );
        assert!(silhouette_alpha(r * 0.3, r) < silhouette_alpha(0.0, r));
        assert!(silhouette_alpha(r * 0.8, r) < silhouette_alpha(r * 0.3, r));
        assert_eq!(silhouette_alpha(r, r), 0.0);
        assert_eq!(silhouette_alpha(r + 10.0, r), 0.0);
    }

    #[test]
    fn distance_transform_measures_from_silhouette() {
        // 5x5 里只有中心一个像素是图标本体
        let mut mask = vec![0u8; 25];
        mask[12] = 255;
        let d = distance_transform(&mask, 5, 5);
        assert_eq!(d[12], 0.0);
        assert!((d[13] - 1.0).abs() < 0.01, "相邻像素距离应为 1");
        assert!((d[14] - 2.0).abs() < 0.01, "隔一格应为 2");
        // 对角相邻：chamfer 给出 √2
        assert!((d[8] - std::f32::consts::SQRT_2).abs() < 0.01);
    }

    #[test]
    fn glow_follows_icon_shape_not_a_circle() {
        // 一条竖线形状的图标（模拟 I 型光标）：同一距离上，左右与上下都应点亮，
        // 但远离线条的角落必须保持透明——这正是"光晕贴形状"而非"圆形光斑"。
        let (cw, ch) = (10u32, 20u32);
        let mut cursor = vec![0u8; (cw * ch * 4) as usize];
        for y in 2..18 {
            let i = ((y * cw + 5) * 4) as usize;
            cursor[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
        let pad = 6u32;
        let glow = build_glow_layer(&cursor, cw, ch, pad, 5.0);
        let w = cw + pad * 2;
        let at = |x: u32, y: u32| glow[((y * w + x) * 4 + 3) as usize];

        // 线条左侧 2px：应有明显光晕
        assert!(at(pad + 5 - 2, pad + 10) > 50, "紧贴图标处应发光");
        // 线条正上方延伸处（x=5,y=0 附近）：也应发光
        assert!(at(pad + 5, pad + 2) > 60, "沿图标走向也应发光");
        // 远离线条的贴图角落：必须完全透明
        assert_eq!(at(0, 0), 0, "远离图标的角落不应有圆形光斑");
        assert_eq!(at(w - 1, ch + pad * 2 - 1), 0);
    }

    #[test]
    fn glow_scales_with_cursor_size() {
        assert_eq!(glow_radius(24), GLOW_RADIUS_MIN, "小指针用最小半径");
        assert!(glow_radius(64) > GLOW_RADIUS_MIN, "高 DPI 大指针应放大光晕");
    }

    #[test]
    fn blend_respects_alpha() {
        let mut dst = [0u8, 0, 0];
        blend_rgba_over_rgb(&mut dst, [255, 255, 255, 0]);
        assert_eq!(dst, [0, 0, 0], "全透明不应改变目标");
        blend_rgba_over_rgb(&mut dst, [255, 255, 255, 255]);
        assert_eq!(dst, [255, 255, 255], "全不透明应完全覆盖");
        let mut half = [0u8, 0, 0];
        blend_rgba_over_rgb(&mut half, [255, 255, 255, 128]);
        assert!((120..=136).contains(&(half[0] as i32)), "半透明应近似取中");
    }

    #[test]
    fn recovers_opaque_transparent_and_partial_pixels() {
        // 黑底与白底渲染相同 → 全不透明，保留原色
        assert_eq!(
            recover_rgba([255, 255, 255], [255, 255, 255]),
            [255, 255, 255, 255]
        );
        // 黑底 0、白底 255 → 该像素完全没被画到
        assert_eq!(recover_rgba([0, 0, 0], [255, 255, 255]), [0, 0, 0, 0]);
        // 半透明像素：黑底约为原色一半，白底接近 255
        let half = recover_rgba([128, 128, 128], [255, 255, 255]);
        assert!((120..=136).contains(&half[3]), "应还原出约一半的 alpha");
        assert!(half[0] >= 250, "颜色应还原为白色");
    }

    #[test]
    fn disabled_compositor_leaves_frame_untouched() {
        let mut frame = vec![7u8; 4 * 4 * 3];
        let mut c = CursorCompositor::new(false, true);
        assert!(c.composite(&mut frame, 4, 4, 0, 0).is_none());
        assert!(frame.iter().all(|&b| b == 7));
    }

    #[test]
    fn restore_rect_puts_back_only_that_area() {
        let fw = 6u32;
        let clean = vec![10u8; (fw as usize) * 4 * 3];
        let mut dirty = clean.clone();
        // 弄脏一个 2x2 区域（帧内 1,1 起）
        dirty[(fw as usize + 1) * 3] = 200;
        dirty[(fw as usize + 2) * 3 + 1] = 200;
        let restored = restore_rect(
            &mut dirty,
            &clean,
            fw,
            Rect {
                x: 1,
                y: 1,
                w: 2,
                h: 2,
            },
        );
        assert!(restored);
        assert_eq!(dirty, clean, "该区域应被完整还原");
    }

    #[test]
    fn restore_rect_rejects_out_of_bounds() {
        let fw = 6u32;
        let clean = vec![0u8; (fw as usize) * 4 * 3];
        let mut dirty = clean.clone();
        assert!(!restore_rect(
            &mut dirty,
            &clean,
            fw,
            Rect {
                x: 5,
                y: 0,
                w: 4,
                h: 2
            }
        ));
    }

    #[test]
    fn sprite_placement_uses_frame_origin() {
        // 帧从屏幕 (100, 50) 开始；指针热点在屏幕 (120, 60)，热点在贴图内 (4, 4)
        let (r, sx, sy) = place_sprite(120, 60, 4, 4, 20, 20, 800, 600, 100, 50).unwrap();
        assert_eq!(
            (r.x, r.y),
            (16, 6),
            "贴图左上角 = 热点 - 贴图内偏移 - 帧原点"
        );
        assert_eq!((sx, sy), (0, 0));
        assert_eq!((r.w, r.h), (20, 20));

        // 指针在帧左上方外侧：只有右下角部分可见
        let (r, sx, sy) = place_sprite(102, 52, 4, 4, 20, 20, 800, 600, 100, 50).unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, 18, 18));
        assert_eq!((sx, sy), (2, 2));

        // 完全在帧外
        assert!(place_sprite(1000, 1000, 4, 4, 20, 20, 800, 600, 100, 50).is_none());
    }

    #[test]
    fn rect_clips_to_frame() {
        let r = Rect {
            x: -4,
            y: -6,
            w: 20,
            h: 20,
        };
        let (clipped, src_x, src_y) = r.clip(10, 10).unwrap();
        assert_eq!((src_x, src_y), (4, 6));
        assert_eq!((clipped.x, clipped.y, clipped.w, clipped.h), (0, 0, 10, 10));

        let outside = Rect {
            x: 100,
            y: 100,
            w: 10,
            h: 10,
        };
        assert!(outside.clip(10, 10).is_none());
    }

    #[test]
    fn over_composites_cursor_on_glow() {
        let glow = [80, 170, 255, 128];
        let cursor = [255, 255, 255, 255];
        assert_eq!(over(cursor, glow), cursor, "不透明指针应原样覆盖光晕");
        let behind = [10, 20, 30, 0];
        assert_eq!(over(glow, behind), glow, "透明底上应保留源色");
    }
}
