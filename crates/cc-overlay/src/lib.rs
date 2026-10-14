//! Ghost-text overlay: a layered, topmost, no-activate, click-through window drawn with
//! Direct2D/DirectWrite. Runs its own thread + message loop; the public methods are
//! thread-safe and just post messages.
//!
//! Contract:
//! - `show_ghost` draws `text` in grey (e.g. #9A9A9A at 100% alpha on a transparent
//!   background) with top-left at screen pixel (x, y), using `font` / `font_px`.
//!   Calling it again replaces the text/position with no flicker.
//! - `show_underline` draws a 2px fading underline (fade over `fade_ms`) at the given rect.
//! - `hide` hides everything immediately.
//! - Window styles: WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE |
//!   WS_EX_TOOLWINDOW, WS_POPUP. Use UpdateLayeredWindow (per-pixel alpha) so the
//!   background is truly transparent. Per-monitor DPI aware.
//! - Must never steal focus or appear in Alt-Tab.
//!
//! Implementation notes:
//! - All coordinates, including `GhostStyle::font_px`, are physical screen pixels. The
//!   process (and the overlay thread) are made per-monitor-v2 DPI aware.
//! - The overlay thread owns two layered windows of the same class: one for ghost text and
//!   one for the underline, so an underline fade can run while a new ghost is showing.
//! - Rendering: each window has a 32bpp premultiplied BGRA top-down DIB section selected into
//!   a memory DC. Ghost text is drawn with DirectWrite through an `ID2D1DCRenderTarget`
//!   (software, 96 DPI so 1 DIP == 1 px, grayscale text AA) bound to that DC, then pushed to
//!   the window with `UpdateLayeredWindow(ULW_ALPHA)`. The underline is a solid 2 px strip
//!   written straight into its DIB and faded with the blend's `SourceConstantAlpha`.
//! - Commands travel over a channel; a single `WM_APP` wake message is posted per batch
//!   (coalesced with an atomic flag), and the overlay thread renders only the newest ghost
//!   in a batch, so streaming updates never queue up stale frames.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, COLORREF, D2DERR_RECREATE_TARGET, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM,
    LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetMonitorInfoW,
    MonitorFromPoint, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, SetThreadDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    PostMessageW, RegisterClassExW, SetTimer, SetWindowPos, ShowWindow, UpdateLayeredWindow,
    HTTRANSPARENT, HWND_TOPMOST, MA_NOACTIVATE, MSG, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WM_APP,
    WM_MOUSEACTIVATE, WM_NCHITTEST, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect { pub x: i32, pub y: i32, pub w: i32, pub h: i32 }

#[derive(Debug, Clone)]
pub struct GhostStyle {
    pub font_family: String,
    pub font_px: f32,
    /// RGB, e.g. (0x9A, 0x9A, 0x9A)
    pub color: (u8, u8, u8),
}

impl Default for GhostStyle {
    fn default() -> Self {
        Self { font_family: "Segoe UI".into(), font_px: 16.0, color: (0x9A, 0x9A, 0x9A) }
    }
}

/// Wakes the overlay thread to drain the command channel.
const WM_APP_WAKE: u32 = WM_APP + 1;
const CLASS_NAME: PCWSTR = w!("CaseComplete.Overlay");
const FADE_TIMER_ID: usize = 1;
/// ~30 fps.
const FADE_FRAME_MS: u32 = 33;
const UNDERLINE_PX: i32 = 2;
/// Soft blue (0x3A, 0x7B, 0xD5), opaque, as a little-endian BGRA pixel.
const UNDERLINE_PIXEL: u32 = 0xFF00_0000 | (0x3A << 16) | (0x7B << 8) | 0xD5;
/// Upper bound on any surface dimension, as a guard against absurd input.
const MAX_DIM: i32 = 16_384;

pub struct Overlay {
    tx: Sender<Cmd>,
    wake_pending: Arc<AtomicBool>,
    /// Ghost window handle, used as the wake target. Stored as an integer so `Overlay` is
    /// `Send + Sync`; the window itself is only touched on the overlay thread.
    wake_hwnd: isize,
    thread: Option<JoinHandle<()>>,
}

impl Overlay {
    /// Spawns the overlay thread and creates the window. Returns when the window exists.
    pub fn new() -> anyhow::Result<Overlay> {
        // Fails harmlessly if the awareness was already set (manifest or an earlier call).
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let (tx, rx) = crossbeam_channel::unbounded::<Cmd>();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<anyhow::Result<isize>>(1);
        let wake_pending = Arc::new(AtomicBool::new(false));
        let thread_wake = Arc::clone(&wake_pending);
        let thread = std::thread::Builder::new()
            .name("cc-overlay".into())
            .spawn(move || overlay_thread(rx, thread_wake, ready_tx))
            .context("spawning overlay thread")?;
        match ready_rx.recv() {
            Ok(Ok(wake_hwnd)) => Ok(Overlay { tx, wake_pending, wake_hwnd, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("overlay thread exited before creating its window"))
            }
        }
    }

    pub fn show_ghost(&self, text: &str, x: i32, y: i32, style: &GhostStyle) {
        self.send(Cmd::Ghost(GhostReq { text: text.to_owned(), x, y, style: style.clone() }));
    }

    pub fn show_underline(&self, rect: Rect, fade_ms: u32) {
        self.send(Cmd::Underline(rect, fade_ms));
    }

    pub fn hide(&self) {
        // Goes through the same ordered queue as `show_ghost`, so a ghost queued before this
        // call can never reappear after it. The overlay thread hides on its next wake.
        self.send(Cmd::Hide);
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_err() {
            return; // overlay thread is gone
        }
        if !self.wake_pending.swap(true, Ordering::SeqCst) && !self.post_wake() {
            self.wake_pending.store(false, Ordering::SeqCst);
        }
    }

    fn post_wake(&self) -> bool {
        let hwnd = HWND(self.wake_hwnd as *mut c_void);
        unsafe { PostMessageW(hwnd, WM_APP_WAKE, WPARAM(0), LPARAM(0)).is_ok() }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        // Always post: Quit must be seen even if a wake is already marked pending.
        self.post_wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum Cmd {
    Ghost(GhostReq),
    Underline(Rect, u32),
    Hide,
    Quit,
}

struct GhostReq {
    text: String,
    x: i32,
    y: i32,
    style: GhostStyle,
}

fn overlay_thread(
    rx: Receiver<Cmd>,
    wake_pending: Arc<AtomicBool>,
    ready: Sender<anyhow::Result<isize>>,
) {
    // Windows created on this thread get physical-pixel coordinates even if the process-wide
    // call above was refused because the host set a different awareness first.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut state = match unsafe { State::new() } {
        Ok(state) => state,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(state.ghost.hwnd.0 as isize));
    drop(ready);
    unsafe { state.run(&rx, &wake_pending) };
    // `state` drops here: windows destroyed, GDI objects released.
}

/// Never activates, never hit-tests; everything else is default.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn register_class() -> anyhow::Result<()> {
    let hinstance = GetModuleHandleW(None).context("GetModuleHandleW")?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance.into(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    if RegisterClassExW(&wc) == 0 {
        let err = GetLastError();
        if err != ERROR_CLASS_ALREADY_EXISTS {
            bail!("RegisterClassExW failed: {err:?}");
        }
    }
    Ok(())
}

unsafe fn create_layered_window() -> anyhow::Result<HWND> {
    let hinstance = GetModuleHandleW(None).context("GetModuleHandleW")?;
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        CLASS_NAME,
        w!("CaseComplete overlay"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        hinstance,
        None,
    )
    .context("CreateWindowExW")?;
    Ok(hwnd)
}

/// Work area of the monitor nearest to (x, y), in physical pixels.
unsafe fn work_area(x: i32, y: i32) -> RECT {
    let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        info.rcWork
    } else {
        RECT { left: -MAX_DIM, top: -MAX_DIM, right: MAX_DIM, bottom: MAX_DIM }
    }
}

/// A memory DC with a 32bpp top-down DIB section selected into it. The DIB only grows.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u8,
    cap_w: i32,
    cap_h: i32,
}

impl Surface {
    unsafe fn new() -> anyhow::Result<Surface> {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            bail!("CreateCompatibleDC failed");
        }
        Ok(Surface {
            dc,
            bitmap: HBITMAP::default(),
            original: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            cap_w: 0,
            cap_h: 0,
        })
    }

    unsafe fn ensure(&mut self, w: i32, h: i32) -> anyhow::Result<()> {
        if w <= self.cap_w && h <= self.cap_h && !self.bits.is_null() {
            return Ok(());
        }
        // Round up so a ghost growing one token at a time does not reallocate every frame.
        let new_w = (w.max(self.cap_w) + 63) & !63;
        let new_h = (h.max(self.cap_h) + 15) & !15;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: new_w,
                biHeight: -new_h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(self.dc, &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .context("CreateDIBSection")?;
        if bits.is_null() {
            let _ = DeleteObject(bitmap);
            bail!("CreateDIBSection returned no pixel memory");
        }
        let previous = SelectObject(self.dc, bitmap);
        if self.original.is_invalid() {
            self.original = previous;
        } else {
            let _ = DeleteObject(self.bitmap);
        }
        self.bitmap = bitmap;
        self.bits = bits.cast();
        self.cap_w = new_w;
        self.cap_h = new_h;
        Ok(())
    }

    fn stride(&self) -> usize {
        self.cap_w as usize * 4
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            if !self.original.is_invalid() {
                SelectObject(self.dc, self.original);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap);
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

/// One layered window plus its backing surface.
struct Layer {
    hwnd: HWND,
    surface: Surface,
}

impl Layer {
    unsafe fn new() -> anyhow::Result<Layer> {
        let surface = Surface::new()?;
        let hwnd = create_layered_window()?;
        Ok(Layer { hwnd, surface })
    }

    /// Pushes the top-left `size` of the surface to the screen at `pos`.
    unsafe fn present(&self, pos: POINT, size: SIZE, alpha: u8) -> anyhow::Result<()> {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let src = POINT { x: 0, y: 0 };
        UpdateLayeredWindow(
            self.hwnd,
            None,
            Some(&pos),
            Some(&size),
            self.surface.dc,
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
        .context("UpdateLayeredWindow")
    }

    /// Shows without activating and re-asserts topmost z-order.
    unsafe fn show(&self) {
        let _ = SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
        );
    }

    unsafe fn hide(&self) {
        let _ = ShowWindow(self.hwnd, SW_HIDE);
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[derive(Clone, Copy)]
struct Fade {
    start: Instant,
    duration: Duration,
    pos: POINT,
    size: SIZE,
}

struct CachedFormat {
    family: String,
    px_bits: u32,
    format: IDWriteTextFormat,
}

/// Everything the overlay thread owns. Lives and dies on that thread.
struct State {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: Option<ID2D1DCRenderTarget>,
    format: Option<CachedFormat>,
    ghost: Layer,
    underline: Layer,
    fade: Option<Fade>,
}

impl State {
    unsafe fn new() -> anyhow::Result<State> {
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
            .context("D2D1CreateFactory")?;
        let dwrite: IDWriteFactory =
            DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).context("DWriteCreateFactory")?;
        register_class()?;
        let ghost = Layer::new()?;
        let underline = Layer::new()?;
        Ok(State { d2d, dwrite, target: None, format: None, ghost, underline, fade: None })
    }

    unsafe fn run(&mut self, rx: &Receiver<Cmd>, wake_pending: &AtomicBool) {
        let mut msg = MSG::default();
        loop {
            // 0 = WM_QUIT, -1 = error; either way the loop is over.
            if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                break;
            }
            if msg.message == WM_APP_WAKE {
                // Clear before draining: a command sent after this point posts a new wake.
                wake_pending.store(false, Ordering::SeqCst);
                if !self.drain(rx) {
                    break;
                }
                continue;
            }
            if msg.message == WM_TIMER
                && msg.hwnd == self.underline.hwnd
                && msg.wParam.0 == FADE_TIMER_ID
            {
                self.fade_tick();
                continue;
            }
            DispatchMessageW(&msg);
        }
    }

    /// Applies every queued command in order. Only the newest ghost in the batch is rendered.
    /// Returns false when the overlay should shut down.
    unsafe fn drain(&mut self, rx: &Receiver<Cmd>) -> bool {
        let mut ghost: Option<GhostReq> = None;
        let mut keep_running = true;
        loop {
            match rx.try_recv() {
                Ok(Cmd::Ghost(req)) => ghost = Some(req),
                Ok(Cmd::Underline(rect, fade_ms)) => {
                    if let Err(e) = self.start_underline(rect, fade_ms) {
                        log::warn!("cc-overlay: underline failed: {e:#}");
                        self.stop_fade();
                        self.underline.hide();
                    }
                }
                Ok(Cmd::Hide) => {
                    ghost = None;
                    self.hide_all();
                }
                Ok(Cmd::Quit) | Err(TryRecvError::Disconnected) => {
                    keep_running = false;
                    ghost = None;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Some(req) = ghost {
            if let Err(e) = self.show_ghost(&req) {
                log::warn!("cc-overlay: ghost render failed: {e:#}");
                self.ghost.hide();
            }
        }
        keep_running
    }

    unsafe fn hide_all(&mut self) {
        self.stop_fade();
        self.ghost.hide();
        self.underline.hide();
    }

    unsafe fn text_format(&mut self, style: &GhostStyle) -> anyhow::Result<IDWriteTextFormat> {
        let px = if style.font_px.is_finite() && style.font_px > 0.0 { style.font_px } else { 16.0 };
        if let Some(cached) = &self.format {
            if cached.family == style.font_family && cached.px_bits == px.to_bits() {
                return Ok(cached.format.clone());
            }
        }
        let family = if style.font_family.trim().is_empty() { "Segoe UI" } else { &style.font_family };
        let family_w: Vec<u16> = family.encode_utf16().chain(Some(0)).collect();
        // Unknown families fall back through DirectWrite's system font fallback.
        let format = self
            .dwrite
            .CreateTextFormat(
                PCWSTR(family_w.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px,
                w!(""),
            )
            .context("CreateTextFormat")?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        self.format = Some(CachedFormat {
            family: style.font_family.clone(),
            px_bits: px.to_bits(),
            format: format.clone(),
        });
        Ok(format)
    }

    unsafe fn render_target(&mut self) -> anyhow::Result<ID2D1DCRenderTarget> {
        if let Some(target) = &self.target {
            return Ok(target.clone());
        }
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            // 96 DPI: one DIP is one physical pixel.
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target = self.d2d.CreateDCRenderTarget(&props).context("CreateDCRenderTarget")?;
        // ClearType needs an opaque destination; on a transparent surface use grayscale AA.
        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        self.target = Some(target.clone());
        Ok(target)
    }

    unsafe fn show_ghost(&mut self, req: &GhostReq) -> anyhow::Result<()> {
        if req.text.is_empty() {
            self.ghost.hide();
            return Ok(());
        }
        let format = self.text_format(&req.style)?;
        let work = work_area(req.x, req.y);
        let work_w = (work.right - work.left).clamp(1, MAX_DIM);
        let work_h = (work.bottom - work.top).clamp(1, MAX_DIM);

        // Measure. '\n' (and "\r\n") are paragraph breaks in DirectWrite, so multi-line works.
        let text_w: Vec<u16> = req.text.encode_utf16().collect();
        let layout = self
            .dwrite
            .CreateTextLayout(&text_w, &format, work_w as f32, work_h as f32)
            .context("CreateTextLayout")?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        let box_w = metrics.widthIncludingTrailingWhitespace.max(metrics.width).max(1.0);
        let box_h = metrics.height.max(1.0);
        // Tighten the layout box so overhangs are measured relative to the text itself.
        layout.SetMaxWidth(box_w)?;
        layout.SetMaxHeight(box_h)?;
        let overhang = layout.GetOverhangMetrics()?;
        let pad = |v: f32| v.max(0.0).ceil() as i32 + 1;
        let (pad_l, pad_t) = (pad(overhang.left), pad(overhang.top));
        let (pad_r, pad_b) = (pad(overhang.right), pad(overhang.bottom));

        let w = (pad_l + box_w.ceil() as i32 + pad_r).min(work_w);
        let h = (pad_t + box_h.ceil() as i32 + pad_b).min(work_h);
        // Text origin lands on (x, y) unless that would push the window off the work area.
        let x = (req.x - pad_l).clamp(work.left, work.right - w);
        let y = (req.y - pad_t).clamp(work.top, work.bottom - h);

        self.ghost.surface.ensure(w, h)?;
        let (r, g, b) = req.style.color;
        let color = D2D1_COLOR_F { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 };
        let transparent = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
        let origin = D2D_POINT_2F { x: pad_l as f32, y: pad_t as f32 };
        let bounds = RECT { left: 0, top: 0, right: w, bottom: h };

        let mut attempt = 0;
        loop {
            let target = self.render_target()?;
            let brush = target.CreateSolidColorBrush(&color, None).context("CreateSolidColorBrush")?;
            target.BindDC(self.ghost.surface.dc, &bounds).context("BindDC")?;
            target.BeginDraw();
            target.Clear(Some(&transparent));
            target.DrawTextLayout(origin, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            match target.EndDraw(None, None) {
                Ok(()) => break,
                Err(e) => {
                    self.target = None;
                    if e.code() == D2DERR_RECREATE_TARGET && attempt == 0 {
                        attempt += 1;
                        continue;
                    }
                    return Err(anyhow!(e).context("EndDraw"));
                }
            }
        }

        self.ghost.present(POINT { x, y }, SIZE { cx: w, cy: h }, 255)?;
        self.ghost.show();
        Ok(())
    }

    unsafe fn start_underline(&mut self, rect: Rect, fade_ms: u32) -> anyhow::Result<()> {
        self.stop_fade();
        if rect.w <= 0 || fade_ms == 0 {
            self.underline.hide();
            return Ok(());
        }
        let w = rect.w.min(MAX_DIM);
        let h = UNDERLINE_PX;
        let pos = POINT { x: rect.x, y: rect.y + rect.h.max(h) - h };
        let size = SIZE { cx: w, cy: h };

        let surface = &mut self.underline.surface;
        surface.ensure(w, h)?;
        // Make sure GDI is done with the bitmap before writing its pixels directly.
        let _ = GdiFlush();
        for row in 0..h as usize {
            let line = surface.bits.add(row * surface.stride()).cast::<u32>();
            std::slice::from_raw_parts_mut(line, w as usize).fill(UNDERLINE_PIXEL);
        }

        self.underline.present(pos, size, 255)?;
        self.underline.show();
        self.fade = Some(Fade {
            start: Instant::now(),
            duration: Duration::from_millis(u64::from(fade_ms)),
            pos,
            size,
        });
        if SetTimer(self.underline.hwnd, FADE_TIMER_ID, FADE_FRAME_MS, None) == 0 {
            bail!("SetTimer failed");
        }
        Ok(())
    }

    unsafe fn fade_tick(&mut self) {
        let Some(fade) = self.fade else {
            self.stop_fade();
            return;
        };
        let t = fade.start.elapsed().as_secs_f32() / fade.duration.as_secs_f32().max(0.001);
        if t >= 1.0 {
            self.stop_fade();
            self.underline.hide();
            return;
        }
        let alpha = ((1.0 - t) * 255.0).round().clamp(0.0, 255.0) as u8;
        if let Err(e) = self.underline.present(fade.pos, fade.size, alpha) {
            log::warn!("cc-overlay: fade frame failed: {e:#}");
            self.stop_fade();
            self.underline.hide();
        }
    }

    unsafe fn stop_fade(&mut self) {
        if self.fade.take().is_some() {
            let _ = KillTimer(self.underline.hwnd, FADE_TIMER_ID);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Overlay>();
    }

    #[test]
    fn underline_pixel_is_soft_blue_bgra() {
        assert_eq!(UNDERLINE_PIXEL.to_le_bytes(), [0xD5, 0x7B, 0x3A, 0xFF]);
    }
}
