//! 桌面悬浮球：置顶、可拖动、单击展开主界面、右键菜单，球体随助手状态变化。
//!
//! Slint 的软件渲染不支持逐像素透明窗口，所以悬浮球用 Win32 分层窗口（UpdateLayeredWindow）
//! 自己逐像素绘制：边缘抗锯齿、阴影和光晕能真正透出桌面。只在助手工作时逐帧刷新，待机时只画一帧。
//! 窗口消息由 Slint（winit）的消息循环分发，所有函数都只能在界面线程调用。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromPoint, MonitorFromWindow,
    SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetCursorPos, GetWindowRect,
    IDC_HAND, KillTimer, LoadCursorW, MF_SEPARATOR, MF_STRING, RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, ULW_ALPHA, UpdateLayeredWindow, WM_CAPTURECHANGED,
    WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

/// 窗口边长（逻辑像素），球体直径 52，四周留出阴影和波纹的空间
const BOX: f32 = 96.0;
const RADIUS: f32 = 26.0;
const TIMER_ID: usize = 1;
/// TrackMouseEvent 的离开通知（这个版本的 windows crate 没有导出）
const WM_MOUSELEAVE: u32 = 0x02A3;

#[derive(Clone, Copy, Debug)]
pub enum BallEvent {
    Open,
    Talk,
    ToggleListening,
    Hide,
    Quit,
    Moved(i32, i32),
}

struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    size: i32,
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Ball {
    hwnd: HWND,
    scale: f32,
    /// 状态码，和界面一致：0 待机 1 正在听 2 思考 3 执行 4 播报 5 已暂停
    mode: i32,
    hover: bool,
    listening: bool,
    visible: bool,
    enabled: bool,
    /// 按下时的光标位置和窗口位置
    down: Option<(POINT, i32, i32)>,
    dragging: bool,
    timer_on: bool,
    started: Instant,
    canvas: Option<Canvas>,
    on_event: Rc<dyn Fn(BallEvent)>,
}

thread_local! {
    static BALL: RefCell<Option<Ball>> = const { RefCell::new(None) };
}

/// 在借用里只改数据，绝不调用 Win32（Win32 调用可能同步回调窗口过程，造成重复借用）
fn with_ball<T>(f: impl FnOnce(&mut Ball) -> T) -> Option<T> {
    BALL.with_borrow_mut(|b| b.as_mut().map(f))
}

fn size_px(scale: f32) -> i32 {
    (BOX * scale).round() as i32
}

/// 创建悬浮球（先隐藏）。pos 是上次保存的窗口位置。
pub fn create(pos: Option<(i32, i32)>, enabled: bool, on_event: impl Fn(BallEvent) + 'static) -> anyhow::Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_HAND)?,
            lpszClassName: w!("VoiceAssistantBall"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            w!("VoiceAssistantBall"),
            w!("VOA 悬浮球"),
            WS_POPUP,
            0,
            0,
            size_px(1.0),
            size_px(1.0),
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        BALL.with_borrow_mut(|b| {
            *b = Some(Ball {
                hwnd,
                scale,
                mode: 0,
                hover: false,
                listening: true,
                visible: false,
                enabled,
                down: None,
                dragging: false,
                timer_on: false,
                started: Instant::now(),
                canvas: None,
                on_event: Rc::new(on_event),
            })
        });
        let n = size_px(scale);
        let (x, y) = match pos {
            Some((x, y)) if on_screen(x + n / 2, y + n / 2) => (x, y),
            _ => default_position(n, scale),
        };
        let _ = SetWindowPos(hwnd, None, x, y, n, n, SWP_NOZORDER | SWP_NOACTIVATE);
        redraw();
    }
    Ok(())
}

fn on_screen(x: i32, y: i32) -> bool {
    unsafe { !MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL).is_invalid() }
}

fn work_area_at(point: POINT) -> RECT {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        info.rcWork
    }
}

/// 默认放在主屏右下角偏上
fn default_position(n: i32, scale: f32) -> (i32, i32) {
    let work = work_area_at(POINT { x: 0, y: 0 });
    (work.right - n - (12.0 * scale) as i32, work.bottom - n - (140.0 * scale) as i32)
}

pub fn set_state(mode: i32) {
    let Some((hwnd, start, stop)) = with_ball(|b| {
        b.mode = mode;
        let active = (1..=4).contains(&mode);
        let start = active && !b.timer_on && b.visible;
        let stop = !active && b.timer_on;
        if start {
            b.timer_on = true;
            b.started = Instant::now();
        }
        if stop {
            b.timer_on = false;
        }
        (b.hwnd, start, stop)
    }) else {
        return;
    };
    unsafe {
        if start {
            SetTimer(Some(hwnd), TIMER_ID, 33, None);
        }
        if stop {
            let _ = KillTimer(Some(hwnd), TIMER_ID);
        }
    }
    redraw();
}

pub fn set_listening(on: bool) {
    with_ball(|b| b.listening = on);
}

/// 是否启用悬浮球（设置里的开关）；关闭时立即隐藏
pub fn set_enabled(on: bool) {
    with_ball(|b| b.enabled = on);
    if !on {
        set_visible(false);
    }
}

pub fn is_enabled() -> bool {
    with_ball(|b| b.enabled).unwrap_or(false)
}

/// 主界面关闭后调用：启用了悬浮球就显示出来
pub fn show_if_enabled() {
    if is_enabled() {
        set_visible(true);
    }
}

pub fn set_visible(visible: bool) {
    let Some((hwnd, mode)) = with_ball(|b| {
        b.visible = visible;
        (b.hwnd, b.mode)
    }) else {
        return;
    };
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
    if visible {
        set_state(mode); // 重新判断是否需要动画
    } else {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_ID);
        }
        with_ball(|b| b.timer_on = false);
    }
}

/// 悬浮球窗口在屏幕上的位置（物理像素）
pub fn rect() -> Option<RECT> {
    let hwnd = with_ball(|b| b.hwnd)?;
    let mut r = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r).ok()? };
    Some(r)
}

/// 悬浮球所在屏幕的工作区（不含任务栏）
pub fn work_area() -> RECT {
    match with_ball(|b| b.hwnd) {
        Some(hwnd) => unsafe {
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(monitor, &mut info);
            info.rcWork
        },
        None => work_area_at(POINT { x: 0, y: 0 }),
    }
}

fn emit(event: BallEvent) {
    if let Some(cb) = with_ball(|b| b.on_event.clone()) {
        cb(event);
    }
}

// ---------------- 绘制 ----------------

fn redraw() {
    // 1. 在借用里准备画布并逐像素绘制
    let job = with_ball(|b| {
        let n = size_px(b.scale);
        if b.canvas.as_ref().map(|c| c.size) != Some(n) {
            b.canvas = unsafe { new_canvas(n) };
        }
        let canvas = b.canvas.as_ref()?;
        let pixels = unsafe { std::slice::from_raw_parts_mut(canvas.bits, (n * n) as usize) };
        let t = b.started.elapsed().as_secs_f32();
        render(pixels, n as usize, b.scale, b.mode, b.hover, t);
        Some((b.hwnd, canvas.dc, n))
    });
    // 2. 借用结束后再交给系统合成
    if let Some(Some((hwnd, dc, n))) = job {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let _ = UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&SIZE { cx: n, cy: n }),
                Some(dc),
                Some(&POINT { x: 0, y: 0 }),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

unsafe fn new_canvas(n: i32) -> Option<Canvas> {
    unsafe {
        let dc = CreateCompatibleDC(None);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                biHeight: -n, // 自上而下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(dc, bitmap.into());
        Some(Canvas { dc, bitmap, old, bits: bits as *mut u32, size: n })
    }
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// 预乘 alpha 的“叠加在上面”
fn over(dst: &mut [f32; 4], color: [f32; 3], alpha: f32) {
    let a = alpha.clamp(0.0, 1.0);
    for i in 0..3 {
        dst[i] = color[i] * a + dst[i] * (1.0 - a);
    }
    dst[3] = a + dst[3] * (1.0 - a);
}

/// 竖直胶囊（声波条）的覆盖率
fn capsule(x: f32, y: f32, cx: f32, half_h: f32, half_w: f32) -> f32 {
    let qy = (y.abs() - half_h).max(0.0);
    let d = ((x - cx).powi(2) + qy * qy).sqrt() - half_w;
    (0.5 - d).clamp(0.0, 1.0)
}

fn render(px: &mut [u32], n: usize, s: f32, mode: i32, hover: bool, t: f32) {
    let c = n as f32 / 2.0;
    let r = RADIUS * s;
    let active = (1..=4).contains(&mode);
    let paused = mode == 5;
    let thinking = mode == 2 || mode == 3;
    let tau = std::f32::consts::TAU;

    // 聆听光：和界面强调色同一色系；暂停时变灰
    let (light, deep) = if paused {
        ([0.66, 0.69, 0.74], [0.37, 0.40, 0.45])
    } else {
        ([0.58, 0.62, 1.0], [0.20, 0.25, 0.86])
    };
    let glow = [0.42, 0.47, 1.0];
    let white = [1.0, 1.0, 1.0];
    // 播报时整体随节奏轻微变亮
    let pulse = if mode == 4 { (0.5 + 0.5 * (t * tau / 0.9).sin()) * 0.10 } else { 0.0 };
    let lift = if hover { 0.08 } else { 0.0 } + pulse;

    // 声波条的半高（相对球半径）
    let bars: [f32; 5] = {
        let base = [0.18, 0.30, 0.42, 0.30, 0.18];
        let mut h = base;
        for (i, v) in h.iter_mut().enumerate() {
            *v = match mode {
                1 => base[i] * (0.45 + 0.55 * ((t * 7.0 + i as f32 * 1.3).sin() * 0.5 + 0.5)),
                4 => base[i] * (0.40 + 0.75 * ((t * 11.0 + i as f32 * 0.9).sin() * 0.5 + 0.5)),
                5 => 0.06,
                _ => base[i],
            };
        }
        h
    };

    for y in 0..n {
        for x in 0..n {
            let fx = x as f32 + 0.5 - c;
            let fy = y as f32 + 0.5 - c;
            let d = (fx * fx + fy * fy).sqrt();
            let mut p = [0.0f32; 4];

            // 1. 柔和投影（向下偏 2px）
            let ds = (fx * fx + (fy - 2.0 * s).powi(2)).sqrt();
            let shadow = ((r + 6.0 * s - ds) / (9.0 * s)).clamp(0.0, 1.0).powi(2) * 0.30;
            over(&mut p, [0.0, 0.0, 0.0], shadow);

            // 2. 光晕：工作时明显，悬停时淡淡一圈
            if active || hover {
                let strength = if active { 0.55 } else { 0.28 };
                let g = (-(d - r).max(0.0) / (8.0 * s)).exp() * strength * if d > r - 1.0 { 1.0 } else { 0.0 };
                over(&mut p, glow, g);
            }

            // 3. 正在听：两圈向外扩散的波纹
            if mode == 1 {
                for k in 0..2 {
                    let phase = (t / 1.6 + k as f32 * 0.5).fract();
                    let ring_r = r + phase * 18.0 * s;
                    let ring = (1.0 - (d - ring_r).abs() / (1.5 * s)).clamp(0.0, 1.0);
                    over(&mut p, light, ring * (1.0 - phase) * 0.75);
                }
            }

            // 4. 球体：左上亮、右下深的渐变 + 高光 + 玻璃感边缘
            let cover = (r - d + 0.5).clamp(0.0, 1.0);
            if cover > 0.0 {
                let k = ((fx + fy) / (2.0 * r) * 0.5 + 0.5).clamp(0.0, 1.0);
                let mut col = mix(light, deep, k);
                col = mix(col, white, lift);
                let (hx, hy) = (fx + 0.36 * r, fy + 0.44 * r);
                let highlight = (-(hx * hx + hy * hy) / (2.0 * (0.30 * r).powi(2))).exp() * 0.40;
                col = mix(col, white, highlight);
                let rim = ((d - (r - 2.5 * s)) / (2.5 * s)).clamp(0.0, 1.0) * 0.22;
                col = mix(col, white, rim);
                over(&mut p, col, cover);

                // 5. 白色图形：思考时是三个依次亮起的点，其余是声波条
                let mut glyph = 0.0f32;
                if thinking {
                    for i in 0..3 {
                        let dx = fx - (i as f32 - 1.0) * 7.5 * s;
                        let dd = (dx * dx + fy * fy).sqrt() - 2.8 * s;
                        let bright = 0.35 + 0.65 * ((t / 1.0 - i as f32 * 0.18) * tau).sin().max(0.0);
                        glyph = glyph.max((0.5 - dd).clamp(0.0, 1.0) * bright);
                    }
                } else {
                    for (i, h) in bars.iter().enumerate() {
                        let bx = (i as f32 - 2.0) * 5.4 * s;
                        glyph = glyph.max(capsule(fx, fy, bx, h * r, 1.45 * s));
                    }
                }
                over(&mut p, white, glyph * 0.96 * cover);
            }

            // 预乘 BGRA
            let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
            px[y * n + x] = (to8(p[3]) << 24) | (to8(p[0]) << 16) | (to8(p[1]) << 8) | to8(p[2]);
        }
    }
}

// ---------------- 交互 ----------------

/// 松手后：限制在屏幕工作区内，靠近左右边缘时吸附过去
fn snap(hwnd: HWND) -> (i32, i32) {
    let mut wr = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut wr);
    }
    let scale = with_ball(|b| b.scale).unwrap_or(1.0);
    let n = wr.right - wr.left;
    let margin = ((n as f32 - 2.0 * RADIUS * scale) / 2.0) as i32; // 窗口边缘到球体边缘
    let gap = (8.0 * scale) as i32;
    let work = unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        info.rcWork
    };
    let (mut x, mut y) = (wr.left, wr.top);
    let snap_dist = (40.0 * scale) as i32;
    if x + margin - work.left < snap_dist {
        x = work.left + gap - margin;
    } else if work.right - (x + n - margin) < snap_dist {
        x = work.right - gap - (n - margin);
    }
    x = x.clamp(work.left - margin, work.right - n + margin);
    y = y.clamp(work.top - margin, work.bottom - n + margin);
    unsafe {
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    (x, y)
}

unsafe fn show_menu(hwnd: HWND) {
    let (listening, busy) = with_ball(|b| (b.listening, (1..=4).contains(&b.mode))).unwrap_or((true, false));
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, 1, w!("打开主界面"));
        // 正在听/思考/回答时，这一项变成停止（和点主界面光球一样）
        let _ = AppendMenuW(menu, MF_STRING, 2, if busy { w!("停止") } else { w!("开始说话") });
        let _ = AppendMenuW(menu, MF_STRING, 3, if listening { w!("暂停语音唤醒") } else { w!("恢复语音唤醒") });
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, 4, w!("隐藏悬浮球"));
        let _ = AppendMenuW(menu, MF_STRING, 5, w!("退出"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 先成为前台窗口，点别处时菜单才会自动关闭
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        match cmd.0 {
            1 => emit(BallEvent::Open),
            2 => emit(BallEvent::Talk),
            3 => emit(BallEvent::ToggleListening),
            4 => emit(BallEvent::Hide),
            5 => emit(BallEvent::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mut wr = RECT::default();
                let _ = GetWindowRect(hwnd, &mut wr);
                with_ball(|b| {
                    b.down = Some((pt, wr.left, wr.top));
                    b.dragging = false;
                });
                SetCapture(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let first_hover = with_ball(|b| !std::mem::replace(&mut b.hover, true)).unwrap_or(false);
                if first_hover {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    redraw();
                }
                if let Some(Some((down, x0, y0, dragging))) = with_ball(|b| b.down.map(|(p, x, y)| (p, x, y, b.dragging))) {
                    let mut pt = POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let (dx, dy) = (pt.x - down.x, pt.y - down.y);
                    if dragging || dx.abs() > 4 || dy.abs() > 4 {
                        with_ball(|b| b.dragging = true);
                        let _ = SetWindowPos(hwnd, None, x0 + dx, y0 + dy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let state = with_ball(|b| {
                    let s = (b.down.is_some(), b.dragging);
                    b.down = None;
                    b.dragging = false;
                    s
                });
                let _ = ReleaseCapture();
                match state {
                    Some((true, true)) => {
                        let (x, y) = snap(hwnd);
                        emit(BallEvent::Moved(x, y));
                    }
                    Some((true, false)) => emit(BallEvent::Open),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                with_ball(|b| {
                    b.down = None;
                    b.dragging = false;
                });
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                with_ball(|b| b.hover = false);
                redraw();
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                show_menu(hwnd);
                LRESULT(0)
            }
            WM_TIMER => {
                redraw();
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let dpi = (wparam.0 & 0xFFFF) as f32;
                with_ball(|b| b.scale = (dpi / 96.0).max(1.0));
                let suggested = &*(lparam.0 as *const RECT);
                let n = size_px((dpi / 96.0).max(1.0));
                let _ = SetWindowPos(hwnd, None, suggested.left, suggested.top, n, n, SWP_NOZORDER | SWP_NOACTIVATE);
                redraw();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
