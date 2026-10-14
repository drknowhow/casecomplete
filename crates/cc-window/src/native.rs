//! The few raw Win32 queries tao does not offer.

use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetWindowRect, SetWindowPos, SystemParametersInfoW,
    GA_ROOTOWNER, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOZORDER,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

use crate::geometry::Area;

/// Work area of the primary monitor (the screen minus the taskbar), in
/// physical pixels. tao makes the process per-monitor DPI aware when the
/// event loop is created, so no virtualisation applies.
pub(crate) fn work_area() -> Option<Area> {
    let mut rect = RECT::default();
    // SAFETY: SPI_GETWORKAREA writes one RECT through the pointer, which is
    // valid for the duration of the call.
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut rect as *mut RECT).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if let Err(e) = result {
        log::warn!("cc-window: SPI_GETWORKAREA failed: {e}");
        return None;
    }
    let area = Area {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    };
    (area.width() > 0 && area.height() > 0).then_some(area)
}

/// Moves and resizes the window so that the part Windows actually draws is
/// exactly `target` (physical pixels, screen coordinates).
///
/// `GetWindowRect` (and so tao's `outer_position` / `set_inner_size`) includes
/// invisible resize borders and, for tao's borderless windows, does not map
/// cleanly from the requested client size. The DWM "extended frame bounds" are
/// the visible rectangle; the difference between the two is measured on the
/// live window and compensated for. Does nothing if the window is not shown
/// yet (DWM has no bounds for it) or on any API failure.
pub(crate) fn fit_visible_rect(hwnd: isize, target: Area) {
    let hwnd = HWND(hwnd as *mut c_void);
    // The second pass catches a change of the invisible borders caused by the first.
    for _ in 0..2 {
        let mut rect = RECT::default();
        let mut visible = RECT::default();
        // SAFETY: both calls write one RECT through a valid pointer.
        let queried = unsafe {
            GetWindowRect(hwnd, &mut rect).is_ok()
                && DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    (&mut visible as *mut RECT).cast::<c_void>(),
                    std::mem::size_of::<RECT>() as u32,
                )
                .is_ok()
        };
        if !queried || visible.right <= visible.left || visible.bottom <= visible.top {
            return;
        }
        let want = Area {
            left: target.left - (visible.left - rect.left),
            top: target.top - (visible.top - rect.top),
            right: target.right + (rect.right - visible.right),
            bottom: target.bottom + (rect.bottom - visible.bottom),
        };
        let have = Area {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        };
        if want == have {
            return;
        }
        // SAFETY: plain call with a handle owned by this thread's window.
        let moved = unsafe {
            SetWindowPos(
                hwnd,
                None,
                want.left,
                want.top,
                want.width(),
                want.height(),
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        if let Err(e) = moved {
            log::warn!("cc-window: SetWindowPos failed: {e}");
            return;
        }
    }
}

/// Whether the foreground window is the window `hwnd` (as returned by tao's
/// `WindowExtWindows::hwnd`) or something it owns (a WebView2 popup, say).
///
/// While Windows is switching windows there briefly is no foreground window;
/// that counts as "still ours" so a transition is never mistaken for focus loss.
pub(crate) fn is_foreground(hwnd: isize) -> bool {
    let ours = HWND(hwnd as *mut c_void);
    // SAFETY: plain queries; both calls accept any window handle value.
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() || foreground == ours {
            return true;
        }
        GetAncestor(foreground, GA_ROOTOWNER) == ours
    }
}
