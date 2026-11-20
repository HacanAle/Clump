//! The widget that sits in the taskbar just left of the tray, like the clock:
//!
//! ```text
//!  (buddy)  5H  54%  ↻ 15:30
//!           7D  72%  ↻ Wed
//! ```
//!
//! A per-pixel-alpha layered window made a child of `Shell_TrayWnd` (explorer's taskbar) and
//! drawn with Direct2D/DirectWrite. It lives on its own thread with its own message loop: a child
//! window in another process shares input with explorer, so this thread must never block.
//! The buddy animates at ~15 fps while someone is at the PC and freezes when the user is idle.

use std::cell::RefCell;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, TrackMouseEvent, LASTINPUTINFO, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::Vector2;

use crate::model::{now, Snapshot, Status};
use crate::scheduler::fmt_local;

const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_MOUSELEAVE_: u32 = 0x02A3;
const TICK_TIMER: usize = 1;
const ANIM_TIMER: usize = 2;
const ANIM_MS: u32 = 83;
/// Stop animating after this long without keyboard/mouse input.
const IDLE_MS: u32 = 120_000;

pub type ScreenRect = (i32, i32, i32, i32);

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Loading,
    Active,
    NoSession,
    Starting,
    SignedOut,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub kind: Kind,
    pub pct: f64,
    pub resets_at: Option<i64>,
    /// Weekly (7-day) limit: percent used and reset time.
    pub week: Option<(f64, Option<i64>)>,
    pub show_left: bool,
    pub visible: bool,
    pub stale: bool,
    pub mascot: bool,
    pub show_resets: bool,
    /// Party animation until this unix time (set right after a session was started).
    pub celebrate_until: i64,
}

impl Model {
    pub fn from_snapshot(s: &Snapshot) -> Self {
        let kind = if s.pinging {
            Kind::Starting
        } else {
            match (s.status, s.session) {
                (Status::Loading, None) => Kind::Loading,
                (Status::SignedOut, _) => Kind::SignedOut,
                (_, Some(l)) if l.resets_at.is_some() => Kind::Active,
                (_, Some(_)) | (Status::Ok, None) => Kind::NoSession,
                _ => Kind::Loading,
            }
        };
        Self {
            kind,
            pct: s.session.map(|l| l.pct).unwrap_or(0.0),
            resets_at: s.session.and_then(|l| l.resets_at),
            week: s.weekly.map(|l| (l.pct, l.resets_at)),
            show_left: s.settings.show_left,
            visible: s.settings.show_widget,
            stale: s.status == Status::Stale,
            mascot: s.settings.mascot,
            show_resets: s.settings.show_resets,
            celebrate_until: s.pings.first().filter(|p| p.ok).map(|p| p.at + 6).unwrap_or(0),
        }
    }
}

/// 0 plenty (≥50% left) · 1 half (25–50%) · 2 low (10–25%) · 3 nearly out (<10%).
pub fn level(used: f64) -> u8 {
    let left = 100.0 - used.clamp(0.0, 100.0);
    if left >= 50.0 {
        0
    } else if left >= 25.0 {
        1
    } else if left >= 10.0 {
        2
    } else {
        3
    }
}

/// Status colour for a level, readable on a dark or light taskbar.
pub fn level_rgb(level: u8, light: bool) -> u32 {
    match (level, light) {
        (0, false) => 0x7BE3AE,
        (1, false) => 0xFFD866,
        (2, false) => 0xFFA566,
        (_, false) => 0xFF8095,
        (0, true) => 0x168A54,
        (1, true) => 0x9A6B00,
        (2, true) => 0xC2550F,
        (_, true) => 0xCC2B52,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mood {
    Happy,
    Calm,
    Worried,
    Panic,
    Sleepy,
    Party,
    Puzzled,
}

pub fn mood(m: &Model, t: i64) -> Mood {
    if m.kind == Kind::Starting || t < m.celebrate_until {
        return Mood::Party;
    }
    match m.kind {
        Kind::Loading | Kind::SignedOut => Mood::Puzzled,
        Kind::NoSession | Kind::Starting => Mood::Sleepy,
        Kind::Active => {
            if m.resets_at.map_or(true, |r| r <= t) || m.pct >= 100.0 {
                return Mood::Sleepy;
            }
            match level(m.pct) {
                0 => Mood::Happy,
                1 => Mood::Calm,
                2 => Mood::Worried,
                _ => Mood::Panic,
            }
        }
    }
}

static MODEL: Mutex<Option<Model>> = Mutex::new(None);
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static SCREEN_RECT: Mutex<Option<ScreenRect>> = Mutex::new(None);
static ON_CLICK: OnceLock<Box<dyn Fn(ScreenRect) + Send + Sync>> = OnceLock::new();

pub fn start(on_click: impl Fn(ScreenRect) + Send + Sync + 'static) {
    let _ = ON_CLICK.set(Box::new(on_click));
    let _ = std::thread::Builder::new().name("taskbar-widget".into()).spawn(thread_main);
}

pub fn update(m: Model) {
    *MODEL.lock().unwrap() = Some(m);
    let h = HWND_RAW.load(Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn light_taskbar() -> bool {
    let mut v: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r == ERROR_SUCCESS && v == 1
}

fn user_idle_ms() -> u32 {
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if GetLastInputInfo(&mut li).as_bool() {
            GetTickCount().wrapping_sub(li.dwTime)
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------- window thread

struct WState {
    hwnd: HWND,
    parent: Option<HWND>,
    model: Option<Model>,
    frame: Option<Frame>,
    hover: bool,
    pressed: bool,
    hop_at: Option<Instant>,
    clock: Instant,
    animating: bool,
    /// Next draw must repaint everything (text, hover), not just the buddy.
    dirty: bool,
    surface: Option<Surface>,
    pos: (i32, i32),
    shown: bool,
}

/// Persistent 32-bit DIB the widget is drawn into; reused every frame.
struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

unsafe fn make_surface(w: i32, h: i32) -> Option<Surface> {
    let dc = CreateCompatibleDC(None);
    if dc.is_invalid() {
        return None;
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let Ok(bmp) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(dc);
        return None;
    };
    let old = SelectObject(dc, bmp.into());
    Some(Surface { dc, bmp, old, w, h })
}

thread_local! {
    static STATE: RefCell<Option<WState>> = const { RefCell::new(None) };
    static GFX: RefCell<Option<Gfx>> = const { RefCell::new(None) };
}

fn thread_main() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("ClumpTaskbarWidget");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!("Clump"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinst.into()),
            None,
        ) else {
            crate::journal::line("taskbar widget: could not create window");
            return;
        };
        HWND_RAW.store(hwnd.0 as isize, Ordering::SeqCst);
        STATE.with(|s| {
            *s.borrow_mut() = Some(WState {
                hwnd,
                parent: None,
                model: None,
                frame: None,
                hover: false,
                pressed: false,
                hop_at: None,
                clock: Instant::now(),
                animating: false,
                dirty: true,
                surface: None,
                pos: (i32::MIN, i32::MIN),
                shown: false,
            })
        });
        SetTimer(Some(hwnd), TICK_TIMER, 1000, None);
        tick();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn with_state<R>(f: impl FnOnce(&mut WState) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn tick() {
    with_state(|st| unsafe { refresh(st) });
}

fn redraw() {
    with_state(|st| unsafe { draw(st) });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp.0 == ANIM_TIMER => {
            redraw();
            LRESULT(0)
        }
        WM_TIMER | WM_APP_UPDATE => {
            tick();
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            with_state(|st| {
                if !st.hover {
                    st.hover = true;
                    st.dirty = true;
                    st.hop_at = Some(Instant::now());
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                }
            });
            redraw();
            LRESULT(0)
        }
        WM_MOUSELEAVE_ => {
            with_state(|st| {
                st.hover = false;
                st.pressed = false;
                st.dirty = true;
            });
            redraw();
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            with_state(|st| {
                st.pressed = true;
                st.dirty = true;
            });
            redraw();
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_RBUTTONUP => {
            let was_pressed = with_state(|st| {
                st.hop_at = Some(Instant::now());
                st.dirty = true;
                std::mem::replace(&mut st.pressed, false)
            })
            .unwrap_or(false);
            redraw();
            let rect = *SCREEN_RECT.lock().unwrap();
            if let (true, Some(cb), Some(r)) = (was_pressed, ON_CLICK.get(), rect) {
                cb(r);
            }
            LRESULT(0)
        }
        // Clicking us must not activate the taskbar (keeps focus rules simple for the panel).
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn hide(st: &mut WState) {
    if st.shown {
        let _ = ShowWindow(st.hwnd, SW_HIDE);
        st.shown = false;
    }
    set_animating(st, false);
    *SCREEN_RECT.lock().unwrap() = None;
}

unsafe fn set_animating(st: &mut WState, on: bool) {
    if on != st.animating {
        st.animating = on;
        if on {
            SetTimer(Some(st.hwnd), ANIM_TIMER, ANIM_MS, None);
        } else {
            let _ = KillTimer(Some(st.hwnd), ANIM_TIMER);
        }
    }
}

/// Attach to the taskbar (again, after an explorer restart), rebuild the text layout if anything
/// changed, redraw, keep the widget docked against the left edge of the tray.
unsafe fn refresh(st: &mut WState) {
    let Some(model) = MODEL.lock().unwrap().clone() else { return };
    let taskbar = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).ok();
    let Some(tb) = taskbar.filter(|_| model.visible) else {
        if taskbar.is_none() {
            st.parent = None;
        }
        return hide(st);
    };
    if st.parent != Some(tb) {
        SetWindowLongPtrW(st.hwnd, GWL_STYLE, (WS_CHILD | WS_CLIPSIBLINGS).0 as isize);
        if SetParent(st.hwnd, Some(tb)).is_err() {
            return;
        }
        st.parent = Some(tb);
        st.frame = None;
        st.pos = (i32::MIN, i32::MIN);
        st.shown = false;
    }
    let mut tbr = RECT::default();
    if GetWindowRect(tb, &mut tbr).is_err() {
        return;
    }
    let (tb_w, tb_h) = (tbr.right - tbr.left, tbr.bottom - tbr.top);
    if tb_h <= 0 || tb_h > tb_w {
        return hide(st); // vertical taskbar: not supported
    }
    let dpi = GetDpiForWindow(tb).max(96);
    let scale = dpi as f32 / 96.0;
    let mut nr = RECT::default();
    let right_edge = match FindWindowExW(Some(tb), None, w!("TrayNotifyWnd"), PCWSTR::null()) {
        Ok(n) if GetWindowRect(n, &mut nr).is_ok() && nr.right > nr.left => nr.left - tbr.left,
        _ => tb_w - (260.0 * scale) as i32,
    };
    let light = light_taskbar();
    let rows = rows(&model, now());
    let key = format!("{rows:?}|{dpi}|{light}|{tb_h}|{}|{}|{}", model.mascot, model.show_resets, model.stale);
    if st.frame.as_ref().map(|f| f.key != key).unwrap_or(true) {
        match build_frame(key, &rows, &model, scale, tb_h, light) {
            Some(f) => {
                st.frame = Some(f);
                st.dirty = true;
            }
            None => return,
        }
    }
    if st.model.as_ref() != Some(&model) {
        st.dirty = true;
    }
    st.model = Some(model.clone());
    let animate = model.mascot && user_idle_ms() < IDLE_MS;
    if st.animating && !animate {
        st.dirty = true; // settle on a still frame
    }
    set_animating(st, animate);
    if st.dirty {
        draw(st);
    }
    let Some((fw, fh)) = st.frame.as_ref().map(|f| (f.w, f.h)) else { return };
    let x = right_edge - fw - (2.0 * scale) as i32;
    let y = (tb_h - fh) / 2;
    let on_top = GetWindow(tb, GW_CHILD).ok() == Some(st.hwnd);
    if (x, y) != st.pos || !st.shown || !on_top {
        let _ = SetWindowPos(st.hwnd, Some(HWND_TOP), x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        st.pos = (x, y);
        st.shown = true;
    }
    *SCREEN_RECT.lock().unwrap() = Some((tbr.left + x, tbr.top + y, fw, fh));
}

// ---------------------------------------------------------------- text content

#[derive(Clone, Debug, PartialEq)]
struct Row {
    label: &'static str,
    value: String,
    reset: String,
    /// Draw the ↻ icon before `reset`.
    icon: bool,
    level: Option<u8>,
}

fn pct_text(used: f64, show_left: bool) -> String {
    let u = used.clamp(0.0, 100.0).round() as i64;
    format!("{}%", if show_left { 100 - u } else { u })
}

fn weekday(t: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(t, 0).single().map(|d| d.format("%a").to_string()).unwrap_or_default()
}

fn rows(m: &Model, t: i64) -> [Row; 2] {
    let live = m.kind == Kind::Active && m.resets_at.map_or(false, |r| r > t);
    let row = |value: String, reset: &str, icon: bool, level: Option<u8>| Row { label: "5H", value, reset: reset.to_string(), icon, level };
    let five = match m.kind {
        Kind::Active if live => {
            let r = fmt_local(m.resets_at.unwrap_or(0));
            row(pct_text(m.pct, m.show_left), &r, true, Some(level(m.pct)))
        }
        Kind::Active | Kind::Starting => row("…".into(), "hi!", false, None),
        Kind::NoSession => row(pct_text(0.0, m.show_left), "idle", false, Some(0)),
        Kind::Loading => row("…".into(), "", false, None),
        Kind::SignedOut => row("—".into(), "", false, None),
    };
    let week = match m.week {
        Some((p, r)) => Row {
            label: "7D",
            value: pct_text(p, m.show_left),
            reset: r.map(|r| if r - t < 86_400 { fmt_local(r) } else { weekday(r) }).unwrap_or_default(),
            icon: r.is_some(),
            level: Some(level(p)),
        },
        None => Row { label: "7D", value: "—".into(), reset: String::new(), icon: false, level: None },
    };
    [five, week]
}

// ---------------------------------------------------------------- drawing

struct Gfx {
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
}

unsafe fn new_gfx() -> windows::core::Result<Gfx> {
    let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let rt = factory.CreateDCRenderTarget(&props)?;
    Ok(Gfx { factory, dwrite, rt })
}

#[derive(Clone, Copy)]
struct Rgba(f32, f32, f32, f32);

impl Rgba {
    fn hex(v: u32, a: f32) -> Self {
        Rgba(((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0, a)
    }
    fn fade(self, k: f32) -> Self {
        Rgba(self.0, self.1, self.2, self.3 * k)
    }
    fn d2d(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F { r: self.0, g: self.1, b: self.2, a: self.3 }
    }
}

/// Everything about the widget that only changes when the numbers/theme/DPI change.
struct Frame {
    key: String,
    w: i32,
    h: i32,
    texts: Vec<(IDWriteTextLayout, f32, f32, Rgba)>,
    /// Buddy box: left, top, size (px).
    mascot: Option<(f32, f32, f32)>,
    /// The mascot picture at exactly the box size (premultiplied, eyes removed).
    bitmap: Option<ID2D1Bitmap>,
    z_format: Option<IDWriteTextFormat>,
    light: bool,
    stale: bool,
    scale: f32,
}

unsafe fn text_format(g: &Gfx, family: PCWSTR, weight: DWRITE_FONT_WEIGHT, px: f32) -> Option<IDWriteTextFormat> {
    g.dwrite.CreateTextFormat(family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, px, w!("en-us")).ok()
}

unsafe fn text_layout(g: &Gfx, fmt: &IDWriteTextFormat, s: &str) -> Option<(IDWriteTextLayout, f32, f32)> {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let layout = g.dwrite.CreateTextLayout(&wide, fmt, 4000.0, 400.0).ok()?;
    let mut m = DWRITE_TEXT_METRICS::default();
    layout.GetMetrics(&mut m).ok()?;
    Some((layout, m.widthIncludingTrailingWhitespace, m.height))
}

unsafe fn build_frame(key: String, rows: &[Row; 2], m: &Model, scale: f32, h: i32, light: bool) -> Option<Frame> {
    GFX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = new_gfx().ok();
        }
        let g = slot.as_ref()?;
        let px = 12.0 * scale;
        let f_label = text_format(g, w!("Segoe UI Variable Text"), DWRITE_FONT_WEIGHT_SEMI_BOLD, px)?;
        let f_value = text_format(g, w!("Segoe UI Variable Text"), DWRITE_FONT_WEIGHT_BOLD, px)?;
        let f_reset = text_format(g, w!("Segoe UI Variable Text"), DWRITE_FONT_WEIGHT_NORMAL, px)?;
        let f_icon = text_format(g, w!("Segoe Fluent Icons"), DWRITE_FONT_WEIGHT_NORMAL, 10.0 * scale)?;
        let dim = if m.stale { 0.6 } else { 1.0 };
        let fg = if light { Rgba::hex(0x000000, 0.9) } else { Rgba::hex(0xFFFFFF, 1.0) }.fade(dim);
        let hf = h as f32;

        let pad = (8.0 * scale).round();
        let (mascot, x0) = if m.mascot {
            let size = (hf * 0.74).round();
            let left = (4.0 * scale).round();
            (Some((left, ((hf - size) / 2.0).round(), size)), left + size + (4.0 * scale).round())
        } else {
            (None, pad)
        };
        let bitmap = mascot.and_then(|(_, _, size)| {
            let n = size as u32;
            let pixels = crate::gauge::bgra_premultiplied(n);
            let props = D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
            };
            g.rt.CreateBitmap(D2D_SIZE_U { width: n, height: n }, Some(pixels.as_ptr() as *const _), n * 4, &props).ok()
        });

        let mut texts = Vec::new();
        let labels: Vec<_> = rows.iter().map(|r| text_layout(g, &f_label, r.label)).collect::<Option<_>>()?;
        let values: Vec<_> = rows.iter().map(|r| text_layout(g, &f_value, &r.value)).collect::<Option<_>>()?;
        let resets: Vec<_> = rows.iter().map(|r| text_layout(g, &f_reset, &r.reset)).collect::<Option<_>>()?;
        let (icon, icon_w, icon_h) = text_layout(g, &f_icon, "\u{E72C}")?;
        let lh = labels[0].2;
        let y0 = ((hf - 2.0 * lh) / 2.0).round();
        let label_w = labels.iter().map(|l| l.1).fold(0.0, f32::max);
        let value_w = values.iter().map(|l| l.1).fold(0.0, f32::max);
        let x_value = x0 + label_w + (5.0 * scale).round();
        let mut right = x_value + value_w;
        let show_resets = m.show_resets && rows.iter().any(|r| !r.reset.is_empty());
        let x_reset = right + (7.0 * scale).round();
        for (i, row) in rows.iter().enumerate() {
            let y = y0 + lh * i as f32;
            let value_color = row.level.map(|l| Rgba::hex(level_rgb(l, light), 1.0).fade(dim)).unwrap_or(fg);
            texts.push((labels[i].0.clone(), x0, y, fg.fade(0.72)));
            texts.push((values[i].0.clone(), x_value, y, value_color));
            if show_resets && !row.reset.is_empty() {
                let mut x = x_reset;
                if row.icon {
                    texts.push((icon.clone(), x, y + ((lh - icon_h) / 2.0).round() + scale * 0.5, fg.fade(0.6)));
                    x += icon_w + (2.0 * scale).round();
                }
                texts.push((resets[i].0.clone(), x, y, fg.fade(0.72)));
                right = right.max(x + resets[i].1);
            }
        }
        let w = (right + pad).ceil() as i32;
        let z_format = text_format(g, w!("Segoe UI Variable Display"), DWRITE_FONT_WEIGHT_BOLD, 8.0 * scale);
        Some(Frame { key, w, h, texts, mascot, bitmap, z_format, light, stale: m.stale, scale })
    })
}

/// Draw the current frame and hand it to the layered window. Animation frames only repaint the
/// buddy's strip (the text pixels stay in the persistent surface), which keeps CPU use tiny.
unsafe fn draw(st: &mut WState) {
    let Some((w, h)) = st.frame.as_ref().map(|f| (f.w, f.h)) else { return };
    if st.surface.as_ref().map_or(true, |s| s.w != w || s.h != h) {
        st.surface = make_surface(w, h);
        st.dirty = true;
    }
    let full = st.dirty || !st.animating;
    let (Some(frame), Some(model), Some(surf)) = (st.frame.as_ref(), st.model.as_ref(), st.surface.as_ref()) else { return };
    let t = if st.animating { st.clock.elapsed().as_secs_f32() } else { 1.0 };
    let mood = mood(model, now());
    let hop = st.hop_at.map(|h| h.elapsed().as_secs_f32()).filter(|e| *e < 0.55).map(|e| (e / 0.55 * PI).sin() * 10.0).unwrap_or(0.0);
    let (hover, pressed, hwnd) = (st.hover, st.pressed, st.hwnd);
    let ok = GFX.with(|cell| {
        let slot = cell.borrow();
        let Some(g) = slot.as_ref() else { return false };
        let rt = &g.rt;
        let hf = h as f32;
        let bg = match (pressed, hover, frame.light) {
            (true, _, false) => Some(Rgba::hex(0xFFFFFF, 0.0419)),
            (true, _, true) => Some(Rgba::hex(0x000000, 0.0241)),
            (false, true, false) => Some(Rgba::hex(0xFFFFFF, 0.0605)),
            (false, true, true) => Some(Rgba::hex(0x000000, 0.0373)),
            _ => None,
        };
        let rc = RECT { left: 0, top: 0, right: w, bottom: h };
        if rt.BindDC(surf.dc, &rc).is_err() {
            return false;
        }
        rt.BeginDraw();
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let clip = match (full, frame.mascot) {
            (false, Some((ox, _, size))) => Some(D2D_RECT_F { left: 0.0, top: 0.0, right: (ox + size + 2.0 * frame.scale).floor(), bottom: hf }),
            _ => None,
        };
        if let Some(c) = &clip {
            rt.PushAxisAlignedClip(c, D2D1_ANTIALIAS_MODE_ALIASED);
        }
        // Alpha 1/255 everywhere so the whole rect takes clicks (alpha 0 is click-through).
        rt.Clear(Some(&Rgba(0.0, 0.0, 0.0, 1.0 / 255.0).d2d()));
        if let Ok(brush) = rt.CreateSolidColorBrush(&Rgba(1.0, 1.0, 1.0, 1.0).d2d(), None) {
            if let Some(c) = bg {
                brush.SetColor(&c.d2d());
                let inset = ((hf - 40.0 * frame.scale) / 2.0).max(1.0);
                let rr = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F { left: 0.5, top: inset, right: w as f32 - 0.5, bottom: hf - inset },
                    radiusX: 4.0 * frame.scale,
                    radiusY: 4.0 * frame.scale,
                };
                rt.FillRoundedRectangle(&rr, &brush);
            }
            if let Some((ox, oy, size)) = frame.mascot {
                let fade = if frame.stale { 0.6 } else { 1.0 };
                let fg = if frame.light { Rgba::hex(0x000000, 0.75) } else { Rgba::hex(0xFFFFFF, 0.85) };
                Buddy { g, rt, brush: &brush, bitmap: frame.bitmap.as_ref(), ox, oy, u: size / 100.0, fade }.draw(mood, t, hop, fg, frame.z_format.as_ref());
            }
            if full {
                for (layout, x, y, color) in &frame.texts {
                    brush.SetColor(&color.d2d());
                    rt.DrawTextLayout(Vector2 { X: x.round(), Y: *y }, layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
            }
        }
        if clip.is_some() {
            rt.PopAxisAlignedClip();
        }
        if rt.EndDraw(None, None).is_err() {
            return false;
        }
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        UpdateLayeredWindow(hwnd, None, None, Some(&SIZE { cx: w, cy: h }), Some(surf.dc), Some(&POINT { x: 0, y: 0 }), COLORREF(0), Some(&blend), ULW_ALPHA).is_ok()
    });
    if ok {
        st.dirty = false;
    }
}

/// The mascot: the user's picture (eyes removed) plus eyes and effects drawn per mood, in the
/// picture's 0–100 design space. Moves in whole pixels so the pixel-art look stays crisp.
struct Buddy<'a> {
    g: &'a Gfx,
    rt: &'a ID2D1DCRenderTarget,
    brush: &'a ID2D1SolidColorBrush,
    bitmap: Option<&'a ID2D1Bitmap>,
    ox: f32,
    oy: f32,
    u: f32,
    fade: f32,
}

const EYE: u32 = 0x000000;
const SWEAT: u32 = 0x7CC8FF;
const SPARK: u32 = 0xFFD24D;

impl Buddy<'_> {
    fn p(&self, x: f32, y: f32) -> Vector2 {
        Vector2 { X: self.ox + x * self.u, Y: self.oy + y * self.u }
    }
    unsafe fn color(&self, rgb: u32, a: f32) {
        self.brush.SetColor(&Rgba::hex(rgb, a * self.fade).d2d());
    }
    unsafe fn rect(&self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let (a, b) = (self.p(x0, y0), self.p(x1, y1));
        self.rt.FillRectangle(&D2D_RECT_F { left: a.X, top: a.Y, right: b.X, bottom: b.Y }, self.brush);
    }
    unsafe fn line(&self, a: (f32, f32), b: (f32, f32), width: f32) {
        self.rt.DrawLine(self.p(a.0, a.1), self.p(b.0, b.1), self.brush, width * self.u, None);
    }
    unsafe fn dot(&self, c: (f32, f32), rx: f32, ry: f32) {
        let e = D2D1_ELLIPSE { point: self.p(c.0, c.1), radiusX: rx * self.u, radiusY: ry * self.u };
        self.rt.FillEllipse(&e, self.brush);
    }
    unsafe fn sparkle(&self, c: (f32, f32), r: f32) {
        let Ok(geom) = self.g.factory.CreatePathGeometry() else { return };
        let Ok(sink) = geom.Open() else { return };
        let k = r * 0.32;
        let pts = [(0.0, -r), (k, -k), (r, 0.0), (k, k), (0.0, r), (-k, k), (-r, 0.0), (-k, -k)];
        sink.BeginFigure(self.p(c.0, c.1 - r), D2D1_FIGURE_BEGIN_FILLED);
        for (x, y) in pts.iter().skip(1) {
            sink.AddLine(self.p(c.0 + x, c.1 + y));
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        if sink.Close().is_ok() {
            self.rt.FillGeometry(&geom, self.brush, None);
        }
    }

    unsafe fn draw(&mut self, mood: Mood, t: f32, hop: f32, fg: Rgba, z_format: Option<&IDWriteTextFormat>) {
        // Whole-body motion (design units), snapped to whole pixels below.
        let (mut dx, mut dy) = (0.0, if (t * 1.6).sin() > 0.55 { -3.0 } else { 0.0 });
        match mood {
            Mood::Party => dy = -(t * 7.0).sin().abs() * 9.0,
            Mood::Panic if t % 2.4 < 0.5 => dx = if (t * 30.0).sin() > 0.0 { 3.0 } else { -3.0 },
            Mood::Sleepy => dy = 0.0,
            _ => {}
        }
        dy -= hop;
        let (ox, oy) = (self.ox, self.oy);
        self.ox = (self.ox + dx * self.u).round();
        self.oy = (self.oy + dy * self.u).round();

        if let Some(bmp) = self.bitmap {
            let a = self.p(0.0, 0.0);
            let b = self.p(100.0, 100.0);
            let dest = D2D_RECT_F { left: a.X, top: a.Y, right: b.X, bottom: b.Y };
            self.rt.DrawBitmap(bmp, Some(&dest), self.fade, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None);
        }

        // Eyes.
        self.color(EYE, 1.0);
        let [l, r] = crate::gauge::EYES;
        let blink = t % 4.3 < 0.14;
        match mood {
            Mood::Sleepy => {
                for e in [l, r] {
                    self.rect(e.0, e.3 - 2.4, e.2, e.3);
                }
            }
            Mood::Party => {
                for e in [l, r] {
                    let cx = (e.0 + e.2) / 2.0;
                    self.line((e.0, e.3), (cx, e.1 + 1.0), 3.0);
                    self.line((cx, e.1 + 1.0), (e.2, e.3), 3.0);
                }
            }
            Mood::Panic => {
                let my = (l.1 + l.3) / 2.0;
                self.line((l.0, l.1), (l.2, my), 3.0);
                self.line((l.2, my), (l.0, l.3), 3.0);
                self.line((r.2, r.1), (r.0, my), 3.0);
                self.line((r.0, my), (r.2, r.3), 3.0);
            }
            _ if blink => {
                for e in [l, r] {
                    let my = (e.1 + e.3) / 2.0;
                    self.rect(e.0, my - 1.2, e.2, my + 1.2);
                }
            }
            _ => {
                for e in [l, r] {
                    self.rect(e.0, e.1, e.2, e.3);
                }
            }
        }

        // Extras.
        match mood {
            Mood::Worried | Mood::Panic => {
                let ph = (t * 0.7) % 1.0;
                self.color(SWEAT, (1.0 - ph) * 0.95);
                self.dot((92.0, 30.0 + ph * 10.0), 3.6, 4.8);
            }
            Mood::Party => {
                for (k, c) in [(6.0, 14.0), (95.0, 4.0), (97.0, 74.0)].iter().enumerate() {
                    let tw = ((t * 6.0 + k as f32 * 2.1).sin() + 1.0) / 2.0;
                    self.color(SPARK, 0.4 + 0.6 * tw);
                    self.sparkle(*c, 5.0 + 4.0 * tw);
                }
            }
            _ => {}
        }
        self.ox = ox;
        self.oy = oy;
        if let (Mood::Sleepy | Mood::Puzzled, Some(fmt)) = (mood, z_format) {
            let glyphs: &[(f32, &str)] = if mood == Mood::Sleepy { &[(0.0, "z"), (0.5, "z")] } else { &[(0.0, "?")] };
            for (offset, s) in glyphs {
                let ph = if mood == Mood::Sleepy { (t * 0.35 + offset) % 1.0 } else { 0.35 };
                let alpha = if mood == Mood::Sleepy { (ph * PI).sin() } else { 0.9 };
                let wide: Vec<u16> = s.encode_utf16().collect();
                let at = self.p(76.0 + ph * 14.0, -6.0 - ph * 18.0);
                let rect = D2D_RECT_F { left: at.X, top: at.Y, right: at.X + 40.0, bottom: at.Y + 40.0 };
                self.brush.SetColor(&fg.fade(alpha * self.fade).d2d());
                self.rt.DrawText(&wide, fmt, &rect, self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(pct: f64, resets_at: Option<i64>, week: Option<(f64, Option<i64>)>, show_left: bool) -> Model {
        Model {
            kind: Kind::Active,
            pct,
            resets_at,
            week,
            show_left,
            visible: true,
            stale: false,
            mascot: true,
            show_resets: true,
            celebrate_until: 0,
        }
    }

    #[test]
    fn rows_show_5h_and_7d() {
        let r = rows(&model(54.4, Some(10_000), Some((72.0, Some(10_000 + 4 * 86_400))), false), 5_000);
        assert_eq!((r[0].label, r[0].value.as_str(), r[0].icon), ("5H", "54%", true));
        assert_eq!((r[1].label, r[1].value.as_str()), ("7D", "72%"));
        assert_eq!(r[1].reset.len(), 3, "weekly reset more than a day away shows the weekday");
        let left = rows(&model(54.4, Some(10_000), Some((72.0, None)), true), 5_000);
        assert_eq!((left[0].value.as_str(), left[1].value.as_str()), ("46%", "28%"));
    }

    #[test]
    fn colour_follows_percent_left() {
        assert_eq!(level(49.0), 0);
        assert_eq!(level(50.1), 1);
        assert_eq!(level(75.5), 2);
        assert_eq!(level(90.5), 3);
    }

    #[test]
    fn moods() {
        assert_eq!(mood(&model(20.0, Some(9_999), None, false), 1), Mood::Happy);
        assert_eq!(mood(&model(80.0, Some(9_999), None, false), 1), Mood::Worried);
        assert_eq!(mood(&model(95.0, Some(9_999), None, false), 1), Mood::Panic);
        assert_eq!(mood(&model(100.0, Some(9_999), None, false), 1), Mood::Sleepy);
        assert_eq!(mood(&model(30.0, Some(50), None, false), 100), Mood::Sleepy, "window over");
        let mut m = model(10.0, Some(9_999), None, false);
        m.celebrate_until = 10;
        assert_eq!(mood(&m, 5), Mood::Party);
    }

    #[test]
    fn past_reset_reads_as_saying_hi() {
        let r = rows(&model(99.0, Some(100), None, false), 200);
        assert_eq!((r[0].value.as_str(), r[0].reset.as_str()), ("…", "hi!"));
    }
}
