//! Where is the caret, what is the focused field, what text is before the caret.
//! Uses UI Automation (IUIAutomation + TextPattern / TextPattern2) with Win32 fallbacks.
//!
//! Contract:
//! - All functions are called from a worker thread (never the hook thread). Each worker
//!   thread must call `init_thread()` once (CoInitializeEx, create IUIAutomation).
//! - `caret_rect`: TextPattern2 caret range -> bounding rect (screen px). Fallback:
//!   GetGUIThreadInfo(rcCaret) + ClientToScreen of hwndCaret. None if unknown.
//! - `text_before_caret(max)`: TextPattern selection range (collapsed) moved back up to
//!   `max` chars, text read. None when the control has no TextPattern.
//! - `focus_info`: focused window's process exe name, whether the control is a password
//!   field (UIA IsPassword) and editable (ValuePattern not read-only / Edit|Document type).
//! - `font_at_caret`: FontName + FontSize text attributes at the caret, if exposed.
//! - Every call must be bounded (well under 20 ms); if UIA hangs on an app, return None.
//!
//! Implementation notes:
//! - COM apartment: the worker joins the MTA. UIA's own guidance is to make client calls
//!   from an MTA thread: an STA thread must pump messages for cross-apartment calls and
//!   UIA's internal callbacks, and our worker has no message loop. If the thread already
//!   is an STA (RPC_E_CHANGED_MODE) we keep it and carry on.
//! - `init_thread` also sets the *thread* DPI awareness to per-monitor v2 so UIA bounding
//!   rects and the Win32 caret fallback come back in physical screen pixels without the
//!   host process needing a DPI manifest.
//! - Bounding: `init_thread` sets IUIAutomation2 connection and transaction timeouts to
//!   200 ms, so a hung provider costs at most ~200 ms once. A call that times out puts the
//!   foreground window on a 2 s cooldown during which UIA is not touched for it (and a
//!   window Windows reports as hung is never touched). No threads are spawned per call.
//! - The caret "caret range" is TextPattern2::GetCaretRange when the caret is active,
//!   else the first selection range collapsed to its end. `caret_rect`,
//!   `text_before_caret` and `font_at_caret` all use that same point, so the text before
//!   the caret and the overlay position always agree.
//! - The thread's IUIAutomation is intentionally leaked at thread exit: Rust runs Windows
//!   TLS destructors under the loader lock, where releasing COM objects can deadlock.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use windows::core::{Interface, BSTR, HRESULT, PWSTR, VARIANT};
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, POINT, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Gdi::{ClientToScreen, MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayUnaccessData,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
    IUIAutomationTextPattern, IUIAutomationTextPattern2, IUIAutomationTextRange,
    IUIAutomationValuePattern, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
    TextUnit_Character, UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_FontNameAttributeId,
    UIA_FontSizeAttributeId, UIA_TextPattern2Id, UIA_TextPatternId, UIA_ValuePatternId,
    UIA_E_TIMEOUT, UIA_PATTERN_ID,
};
use windows::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetDpiForMonitor, GetDpiForWindow,
    GetWindowDpiAwarenessContext, LogicalToPhysicalPointForPerMonitorDPI,
    SetThreadDpiAwarenessContext, DPI_AWARENESS_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongW,
    GetWindowThreadProcessId, IsHungAppWindow, ES_PASSWORD, GUITHREADINFO, GWL_STYLE,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect { pub x: i32, pub y: i32, pub w: i32, pub h: i32 }

#[derive(Debug, Clone)]
pub struct FocusInfo {
    pub hwnd: isize,
    pub exe: String,
    pub is_password: bool,
    pub is_editable: bool,
    /// UIA control type id (Edit=50004, Document=50030, ...) if known.
    pub control_type: Option<i32>,
}

/// UIA connection + transaction timeout set on IUIAutomation2.
const UIA_TIMEOUT_MS: u32 = 200;
/// After a UIA timeout, leave that foreground window alone for this long.
const HANG_COOLDOWN: Duration = Duration::from_secs(2);
/// Calls slower than this are logged (the contract budget).
const SLOW_CALL: Duration = Duration::from_millis(20);

// VARENUM values we inspect on attribute VARIANTs.
const VT_I2: u16 = 2;
const VT_I4: u16 = 3;
const VT_R4: u16 = 4;
const VT_R8: u16 = 5;
const VT_BSTR: u16 = 8;
const VT_INT: u16 = 22;

thread_local! {
    // ManuallyDrop: never released by the TLS destructor (see module notes).
    static UIA: RefCell<Option<ManuallyDrop<IUIAutomation>>> = const { RefCell::new(None) };
    /// Set by `fail` when a call in the current `with_uia` scope timed out.
    static TIMED_OUT: Cell<bool> = const { Cell::new(false) };
    /// (foreground hwnd, skip UIA until).
    static COOLDOWN: Cell<Option<(isize, Instant)>> = const { Cell::new(None) };
}

/// Call once per worker thread before any other function.
///
/// Idempotent. Joins the MTA, switches this thread to per-monitor-v2 DPI awareness and
/// creates the thread's IUIAutomation (CUIAutomation8, falling back to CUIAutomation).
pub fn init_thread() -> anyhow::Result<()> {
    if UIA.with(|u| u.borrow().is_some()) {
        return Ok(());
    }
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr == RPC_E_CHANGED_MODE {
            log::debug!("init_thread: thread is already an STA; using it");
        } else {
            hr.ok().context("CoInitializeEx(COINIT_MULTITHREADED)")?;
        }

        // Load-bearing: without it UIA returns DPI-virtualised rects (verified: 125% monitor
        // gave 454,270 instead of the physical 568,337).
        if SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).0.is_null() {
            log::debug!("init_thread: SetThreadDpiAwarenessContext(PMv2) failed; rects may be DPI-virtualised");
        }

        // CUIAutomation8 is the Windows 8+ object; it is the one that implements
        // IUIAutomation2 (timeouts). The original CUIAutomation is the fallback.
        let uia: IUIAutomation = match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) {
            Ok(u) => u,
            Err(e) => {
                log::debug!("init_thread: CUIAutomation8 unavailable ({e}); trying CUIAutomation");
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                    .context("CoCreateInstance(CUIAutomation)")?
            }
        };

        match uia.cast::<IUIAutomation2>() {
            Ok(u2) => {
                if let Err(e) = u2.SetConnectionTimeout(UIA_TIMEOUT_MS) {
                    log::debug!("init_thread: SetConnectionTimeout failed: {e}");
                }
                if let Err(e) = u2.SetTransactionTimeout(UIA_TIMEOUT_MS) {
                    log::debug!("init_thread: SetTransactionTimeout failed: {e}");
                }
            }
            Err(e) => log::debug!(
                "init_thread: IUIAutomation2 unavailable ({e}); only the hung-window guard bounds UIA calls"
            ),
        }

        UIA.with(|u| *u.borrow_mut() = Some(ManuallyDrop::new(uia)));
    }
    Ok(())
}

/// Foreground window, its process exe (file name, lowercase) and what UIA says about the
/// focused element. None only when there is no foreground window. If UIA cannot answer
/// (not initialised, provider hung or failing) the result has `control_type: None`,
/// `is_editable: false` and `is_password` from the Win32 ES_PASSWORD style.
pub fn focus_info() -> Option<FocusInfo> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            log::debug!("focus_info: no foreground window");
            return None;
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(fg, Some(&mut pid));
        let mut exe = process_exe_name(pid).unwrap_or_default();

        let uia_part = with_uia("focus_info", |uia| {
            let el = ok("GetFocusedElement", uia.GetFocusedElement())?;
            let ct = ok("CurrentControlType", el.CurrentControlType())?;
            let is_password = ok("CurrentIsPassword", el.CurrentIsPassword())?.as_bool();
            let hwnd = ok("CurrentNativeWindowHandle", el.CurrentNativeWindowHandle())
                .filter(|h| !h.0.is_null())
                .unwrap_or(fg);

            let is_editable = (ct == UIA_EditControlTypeId || ct == UIA_DocumentControlTypeId)
                && match pattern::<IUIAutomationValuePattern>(&el, UIA_ValuePatternId, "ValuePattern") {
                    Ok(Some(vp)) => ok("ValuePattern.CurrentIsReadOnly", vp.CurrentIsReadOnly())
                        .is_some_and(|ro| !ro.as_bool()),
                    Ok(None) => true,
                    Err(()) => false,
                };

            // UWP apps: the frame window belongs to ApplicationFrameHost; the element knows
            // the real process.
            let real_exe = if exe == "applicationframehost.exe" {
                ok("CurrentProcessId", el.CurrentProcessId())
                    .and_then(|p| process_exe_name(p as u32))
            } else {
                None
            };
            Some((hwnd, ct.0, is_password, is_editable, real_exe))
        });

        Some(match uia_part {
            Some((hwnd, ct, is_password, is_editable, real_exe)) => {
                if let Some(e) = real_exe {
                    exe = e;
                }
                FocusInfo { hwnd: hwnd.0 as isize, exe, is_password, is_editable, control_type: Some(ct) }
            }
            None => {
                let focus = gui_thread_info(tid)
                    .map(|g| g.hwndFocus)
                    .filter(|h| !h.0.is_null())
                    .unwrap_or(fg);
                FocusInfo {
                    hwnd: focus.0 as isize,
                    exe,
                    is_password: win32_is_password(focus),
                    is_editable: false,
                    control_type: None,
                }
            }
        })
    }
}

/// Caret rectangle in physical screen pixels: `x,y` = top-left of the caret, `h` = line
/// height, `w` = caret width (at least 1). None if neither UIA nor the Win32 caret knows.
pub fn caret_rect() -> Option<Rect> {
    let from_uia = with_uia("caret_rect", |uia| unsafe {
        let el = ok("GetFocusedElement", uia.GetFocusedElement())?;
        let caret = caret_range(&el)?;
        rect_at_caret(&caret)
    });
    from_uia.or_else(|| unsafe { gui_thread_caret() })
}

/// Up to `max_chars` characters immediately before the caret, line breaks normalised to
/// "\n". None when the focused control has no TextPattern, or is a password field.
pub fn text_before_caret(max_chars: usize) -> Option<String> {
    with_uia("text_before_caret", |uia| unsafe {
        let el = ok("GetFocusedElement", uia.GetFocusedElement())?;
        if ok("CurrentIsPassword", el.CurrentIsPassword())?.as_bool() {
            log::debug!("text_before_caret: password field, not reading");
            return None;
        }
        // A fresh range object (GetCaretRange / selection element), so no Clone needed.
        let range = caret_range(&el)?;
        if max_chars == 0 {
            return Some(String::new());
        }
        let n = i32::try_from(max_chars).unwrap_or(i32::MAX);
        // Providers clamp at the document start.
        ok(
            "MoveEndpointByUnit(Start)",
            range.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -n),
        )?;
        let text = ok("GetText", range.GetText(-1))?.to_string();
        Some(tail_chars(&normalise_newlines(&text), max_chars))
    })
}

/// (font family, size in px)
///
/// Size: UIA FontSize is in points; px = pt * dpi / 72 with the effective DPI of the
/// monitor the foreground window is on (physical px, matching `caret_rect`).
pub fn font_at_caret() -> Option<(String, f32)> {
    with_uia("font_at_caret", |uia| unsafe {
        let el = ok("GetFocusedElement", uia.GetFocusedElement())?;
        let caret = caret_range(&el)?;

        // Degenerate range first; many providers only answer for a real character, so
        // then try the character before the caret (typed text inherits its style), then
        // the one after.
        let mut found = font_of(&caret);
        if found.is_none() && !timed_out() {
            found = neighbour_char(&caret, false).and_then(|r| font_of(&r));
        }
        if found.is_none() && !timed_out() {
            found = neighbour_char(&caret, true).and_then(|r| font_of(&r));
        }
        let (name, pt) = found?;
        let dpi = monitor_dpi(GetForegroundWindow());
        Some((name, (pt * f64::from(dpi) / 72.0) as f32))
    })
}

// ---------------------------------------------------------------------------------------
// Guarded UIA scope + error plumbing

/// Runs UIA work on this thread's IUIAutomation. Returns None (without touching UIA) when
/// `init_thread` was not called, the foreground window is hung, or it is cooling down
/// after a timeout. Never panics.
fn with_uia<T>(what: &'static str, f: impl FnOnce(&IUIAutomation) -> Option<T>) -> Option<T> {
    let uia = UIA.with(|u| u.borrow().as_ref().map(|u| IUIAutomation::clone(u)));
    let Some(uia) = uia else {
        log::debug!("{what}: init_thread() not called on this thread");
        return None;
    };

    let fg = unsafe { GetForegroundWindow() };
    let fg_key = fg.0 as isize;
    if !fg.0.is_null() {
        if unsafe { IsHungAppWindow(fg) }.as_bool() {
            log::debug!("{what}: foreground window {fg_key:#x} is hung; skipping UIA");
            return None;
        }
        if let Some((h, until)) = COOLDOWN.get() {
            if h == fg_key && Instant::now() < until {
                log::debug!("{what}: window {fg_key:#x} timed out recently; skipping UIA");
                return None;
            }
        }
    }

    TIMED_OUT.set(false);
    let start = Instant::now();
    let out = catch_unwind(AssertUnwindSafe(|| f(&uia))).unwrap_or_else(|_| {
        log::debug!("{what}: panic inside UIA call");
        None
    });
    let took = start.elapsed();
    if TIMED_OUT.get() {
        log::debug!("{what}: UIA timed out; cooling window {fg_key:#x} down for {HANG_COOLDOWN:?}");
        COOLDOWN.set(Some((fg_key, Instant::now() + HANG_COOLDOWN)));
    }
    if took > SLOW_CALL {
        log::debug!("{what}: slow UIA call ({took:?})");
    }
    out
}

fn timed_out() -> bool {
    TIMED_OUT.get()
}

fn is_timeout(hr: HRESULT) -> bool {
    const HRESULT_ERROR_TIMEOUT: u32 = 0x8007_05B4;
    const RPC_E_TIMEOUT: u32 = 0x8001_011F;
    matches!(hr.0 as u32, UIA_E_TIMEOUT | HRESULT_ERROR_TIMEOUT | RPC_E_TIMEOUT)
}

fn fail(what: &str, e: &windows::core::Error) {
    if is_timeout(e.code()) {
        TIMED_OUT.set(true);
    }
    log::debug!("{what} failed: {:#010x} {}", e.code().0 as u32, e.message());
}

fn ok<T>(what: &str, r: windows::core::Result<T>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            fail(what, &e);
            None
        }
    }
}

/// Ok(Some) = supported, Ok(None) = not supported, Err = the call itself failed.
unsafe fn pattern<T: Interface>(
    el: &IUIAutomationElement,
    id: UIA_PATTERN_ID,
    what: &str,
) -> Result<Option<T>, ()> {
    match el.GetCurrentPatternAs::<T>(id) {
        Ok(p) => Ok(Some(p)),
        // S_OK + null pointer: pattern not supported.
        Err(e) if e.code().is_ok() => {
            log::debug!("{what}: not supported by focused element");
            Ok(None)
        }
        Err(e) => {
            fail(what, &e);
            Err(())
        }
    }
}

// ---------------------------------------------------------------------------------------
// Text ranges

/// The caret as a degenerate range: TextPattern2 caret range if the caret is active,
/// else the first selection range collapsed to its end. None if no TextPattern.
unsafe fn caret_range(el: &IUIAutomationElement) -> Option<IUIAutomationTextRange> {
    let tp2 = pattern::<IUIAutomationTextPattern2>(el, UIA_TextPattern2Id, "TextPattern2").ok()?;
    if let Some(tp2) = tp2 {
        let mut active = BOOL(0);
        if let Some(r) = ok("GetCaretRange", tp2.GetCaretRange(&mut active)) {
            if active.as_bool() {
                return Some(r);
            }
            log::debug!("GetCaretRange: caret not active; using selection");
        }
        if timed_out() {
            return None;
        }
    }

    let tp = pattern::<IUIAutomationTextPattern>(el, UIA_TextPatternId, "TextPattern").ok()??;
    let sel = ok("TextPattern.GetSelection", tp.GetSelection())?;
    if ok("selection.Length", sel.Length())? < 1 {
        log::debug!("TextPattern.GetSelection: empty selection array");
        return None;
    }
    let range = ok("selection.GetElement(0)", sel.GetElement(0))?;
    ok(
        "collapse selection to end",
        range.MoveEndpointByRange(TextPatternRangeEndpoint_Start, &range, TextPatternRangeEndpoint_End),
    )?;
    Some(range)
}

/// A one-character range next to the degenerate `caret`: after it if `after`, else before.
unsafe fn neighbour_char(caret: &IUIAutomationTextRange, after: bool) -> Option<IUIAutomationTextRange> {
    let r = ok("TextRange.Clone", caret.Clone())?;
    let (endpoint, count) =
        if after { (TextPatternRangeEndpoint_End, 1) } else { (TextPatternRangeEndpoint_Start, -1) };
    let moved = ok("MoveEndpointByUnit", r.MoveEndpointByUnit(endpoint, TextUnit_Character, count))?;
    (moved == count).then_some(r)
}

/// Caret rect from a degenerate range at the caret.
unsafe fn rect_at_caret(caret: &IUIAutomationTextRange) -> Option<Rect> {
    // 1. The range's own rects: TextPattern2 providers usually report the caret itself.
    let own = bounding_rects(caret)?;
    if let Some(r) = own.iter().rev().find(|r| r.h > 0) {
        log::trace!("caret_rect: caret range rect {r:?}");
        return Some(Rect { w: r.w.max(1), ..*r });
    }
    if timed_out() {
        return None;
    }

    // 2. ExpandToEnclosingUnit(Character): normally the character after the caret (caret =
    //    its left edge); at the end of the document some providers expand backwards
    //    (caret = right edge), detected with CompareEndpoints.
    let mut line_break_after: Option<Rect> = None;
    if let Some(ch) = ok("TextRange.Clone", caret.Clone()) {
        if ok("ExpandToEnclosingUnit", ch.ExpandToEnclosingUnit(TextUnit_Character)).is_some() {
            let backwards = ok(
                "CompareEndpoints",
                ch.CompareEndpoints(TextPatternRangeEndpoint_Start, caret, TextPatternRangeEndpoint_Start),
            )
            .map(|c| c < 0);
            let text = ok("GetText", ch.GetText(4)).map(|b| b.to_string()).unwrap_or_default();
            let rects = bounding_rects(&ch).unwrap_or_default();
            match backwards {
                Some(false) if is_real_char(&text) => {
                    if let Some(r) = rects.iter().find(|r| r.h > 0) {
                        log::trace!("caret_rect: left edge of next char {text:?} {r:?}");
                        return Some(Rect { w: 1, ..*r });
                    }
                }
                Some(true) if is_real_char(&text) => {
                    if let Some(r) = rects.iter().rev().find(|r| r.h > 0) {
                        log::trace!("caret_rect: right edge of expanded char {text:?} {r:?}");
                        return Some(Rect { x: r.x + r.w, w: 1, ..*r });
                    }
                }
                Some(false) if !text.is_empty() => {
                    line_break_after = rects.iter().find(|r| r.h > 0).map(|r| Rect { w: 1, ..*r });
                }
                _ => {}
            }
        }
    }
    if timed_out() {
        return None;
    }

    // 3. Character before the caret (caret at end of line / document): its right edge.
    if let Some(prev) = neighbour_char(caret, false) {
        let text = ok("GetText", prev.GetText(4)).map(|b| b.to_string()).unwrap_or_default();
        if is_real_char(&text) {
            if let Some(r) = bounding_rects(&prev).unwrap_or_default().iter().rev().find(|r| r.h > 0) {
                log::trace!("caret_rect: right edge of previous char {text:?} {r:?}");
                return Some(Rect { x: r.x + r.w, w: 1, ..*r });
            }
        }
    }

    // 4. Caret on an empty line: the line break after it is all we have.
    if line_break_after.is_some() {
        log::trace!("caret_rect: line break after caret {line_break_after:?}");
    }
    line_break_after
}

/// Text of a one-character range that is a visible character (not empty, not a line break).
fn is_real_char(text: &str) -> bool {
    !text.is_empty() && !text.chars().all(|c| c == '\r' || c == '\n')
}

/// TextRange::GetBoundingRectangles as rects (physical screen px for a PMv2 thread).
unsafe fn bounding_rects(range: &IUIAutomationTextRange) -> Option<Vec<Rect>> {
    let psa = ok("GetBoundingRectangles", range.GetBoundingRectangles())?;
    if psa.is_null() {
        return Some(Vec::new());
    }
    let vals = read_f64_safearray(psa);
    if let Err(e) = SafeArrayDestroy(psa) {
        log::debug!("SafeArrayDestroy failed: {e}");
    }
    let vals = vals?;
    Some(
        vals.chunks_exact(4)
            .map(|c| Rect {
                x: c[0].round() as i32,
                y: c[1].round() as i32,
                w: c[2].round() as i32,
                h: c[3].round() as i32,
            })
            .collect(),
    )
}

unsafe fn read_f64_safearray(psa: *const SAFEARRAY) -> Option<Vec<f64>> {
    if (*psa).cDims != 1 || (*psa).cbElements != 8 {
        log::debug!("GetBoundingRectangles: unexpected SAFEARRAY shape");
        return None;
    }
    let lb = ok("SafeArrayGetLBound", SafeArrayGetLBound(psa, 1))?;
    let ub = ok("SafeArrayGetUBound", SafeArrayGetUBound(psa, 1))?;
    let n = (i64::from(ub) - i64::from(lb) + 1).max(0) as usize;
    if n == 0 {
        return Some(Vec::new());
    }
    let mut data: *mut c_void = std::ptr::null_mut();
    ok("SafeArrayAccessData", SafeArrayAccessData(psa, &mut data))?;
    let vals = if data.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data as *const f64, n).to_vec()
    };
    if let Err(e) = SafeArrayUnaccessData(psa) {
        log::debug!("SafeArrayUnaccessData failed: {e}");
    }
    Some(vals)
}

/// (FontName, FontSize in points) of `range`, if both are exposed as plain values
/// (not the UIA "not supported" / "mixed" sentinels).
unsafe fn font_of(range: &IUIAutomationTextRange) -> Option<(String, f64)> {
    let name = ok("GetAttributeValue(FontName)", range.GetAttributeValue(UIA_FontNameAttributeId))?;
    let name = variant_string(&name)?;
    let size = ok("GetAttributeValue(FontSize)", range.GetAttributeValue(UIA_FontSizeAttributeId))?;
    let size = variant_f64(&size)?;
    (size.is_finite() && size > 0.0).then_some((name, size))
}

fn vt(v: &VARIANT) -> u16 {
    unsafe { v.as_raw().Anonymous.Anonymous.vt }
}

fn variant_string(v: &VARIANT) -> Option<String> {
    if vt(v) != VT_BSTR {
        log::debug!("FontName: unsupported or mixed (vt={})", vt(v));
        return None;
    }
    BSTR::try_from(v).ok().map(|b| b.to_string()).filter(|s| !s.is_empty())
}

fn variant_f64(v: &VARIANT) -> Option<f64> {
    match vt(v) {
        VT_R8 | VT_R4 | VT_I4 | VT_I2 | VT_INT => f64::try_from(v).ok(),
        other => {
            log::debug!("FontSize: unsupported or mixed (vt={other})");
            None
        }
    }
}

fn normalise_newlines(s: &str) -> String {
    // "\r\n" -> "\n", then bare "\r" (RichEdit paragraph marks) -> "\n".
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_owned()
    } else {
        s.chars().skip(n - max).collect()
    }
}

// ---------------------------------------------------------------------------------------
// Win32 helpers

unsafe fn gui_thread_info(tid: u32) -> Option<GUITHREADINFO> {
    if tid == 0 {
        return None;
    }
    let mut gti = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    ok("GetGUIThreadInfo", GetGUIThreadInfo(tid, &mut gti))?;
    Some(gti)
}

/// GetGUIThreadInfo caret of the foreground thread, mapped to physical screen px.
unsafe fn gui_thread_caret() -> Option<Rect> {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return None;
    }
    let gti = gui_thread_info(GetWindowThreadProcessId(fg, None))?;
    let hwnd = gti.hwndCaret;
    let rc = gti.rcCaret;
    if hwnd.0.is_null() || rc.bottom <= rc.top {
        log::debug!("caret_rect: no Win32 caret on the foreground thread");
        return None;
    }
    let tl = client_to_physical(hwnd, POINT { x: rc.left, y: rc.top })?;
    let br = client_to_physical(hwnd, POINT { x: rc.right, y: rc.bottom })?;
    let r = Rect { x: tl.x, y: tl.y, w: (br.x - tl.x).max(1), h: br.y - tl.y };
    log::trace!("caret_rect: GetGUIThreadInfo caret {r:?}");
    (r.h > 0).then_some(r)
}

/// Client point of `hwnd` (in that window's own DPI space, as rcCaret is) to physical
/// screen pixels.
unsafe fn client_to_physical(hwnd: HWND, mut pt: POINT) -> Option<POINT> {
    let ctx = GetWindowDpiAwarenessContext(hwnd);
    // Do ClientToScreen in the window's own awareness so the client offset is not
    // reinterpreted at a different scale.
    let prev = SetThreadDpiAwarenessContext(ctx);
    let mapped = ClientToScreen(hwnd, &mut pt).as_bool();
    if !prev.0.is_null() {
        let _ = SetThreadDpiAwarenessContext(prev);
    }
    if !mapped {
        log::debug!("ClientToScreen failed for hwnd {:#x}", hwnd.0 as isize);
        return None;
    }
    // Per-monitor-aware windows already work in physical px; others need lifting.
    if GetAwarenessFromDpiAwarenessContext(ctx) != DPI_AWARENESS_PER_MONITOR_AWARE
        && !LogicalToPhysicalPointForPerMonitorDPI(hwnd, &mut pt).as_bool()
    {
        log::debug!("LogicalToPhysicalPointForPerMonitorDPI failed; using logical point");
    }
    Some(pt)
}

unsafe fn process_exe_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let h = ok("OpenProcess", OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid))?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
    if let Err(e) = CloseHandle(h) {
        log::debug!("CloseHandle failed: {e}");
    }
    ok("QueryFullProcessImageNameW", r)?;
    let path = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
    Some(name.to_lowercase())
}

/// Win32 Edit / RichEdit with ES_PASSWORD (used only when UIA cannot answer).
unsafe fn win32_is_password(hwnd: HWND) -> bool {
    let mut cls = [0u16; 128];
    let n = GetClassNameW(hwnd, &mut cls);
    if n <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&cls[..n as usize]).to_ascii_lowercase();
    class.contains("edit") && (GetWindowLongW(hwnd, GWL_STYLE) & ES_PASSWORD) != 0
}

/// Effective DPI of the monitor `hwnd` is on (falls back to the window DPI, then 96).
unsafe fn monitor_dpi(hwnd: HWND) -> u32 {
    let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let (mut x, mut y) = (0u32, 0u32);
    if !mon.0.is_null() && GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() && x > 0 {
        return x;
    }
    match GetDpiForWindow(hwnd) {
        0 => 96,
        d => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_are_normalised() {
        assert_eq!(normalise_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn tail_keeps_last_chars() {
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(tail_chars("hi", 10), "hi");
        assert_eq!(tail_chars("hi", 0), "");
    }

    #[test]
    fn real_char_detection() {
        assert!(is_real_char("a"));
        assert!(!is_real_char("\r"));
        assert!(!is_real_char("\r\n"));
        assert!(!is_real_char(""));
    }

    #[test]
    fn uninitialised_thread_returns_none() {
        // No init_thread() on this test thread: UIA paths must not panic.
        assert!(text_before_caret(10).is_none());
        assert!(font_at_caret().is_none());
    }
}
