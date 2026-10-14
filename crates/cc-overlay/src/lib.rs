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

pub struct Overlay {
    _private: (),
}

impl Overlay {
    /// Spawns the overlay thread and creates the window. Returns when the window exists.
    pub fn new() -> anyhow::Result<Overlay> {
        todo!("cc-overlay: create layered window on its own thread")
    }
    pub fn show_ghost(&self, text: &str, x: i32, y: i32, style: &GhostStyle) {
        let _ = (text, x, y, style);
        todo!()
    }
    pub fn show_underline(&self, rect: Rect, fade_ms: u32) {
        let _ = (rect, fade_ms);
        todo!()
    }
    pub fn hide(&self) {
        todo!()
    }
}
