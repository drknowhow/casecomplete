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

/// Call once per worker thread before any other function.
pub fn init_thread() -> anyhow::Result<()> {
    todo!("cc-context: CoInitializeEx + CUIAutomation")
}

pub fn focus_info() -> Option<FocusInfo> {
    todo!()
}

pub fn caret_rect() -> Option<Rect> {
    todo!()
}

pub fn text_before_caret(max_chars: usize) -> Option<String> {
    let _ = max_chars;
    todo!()
}

/// (font family, size in px)
pub fn font_at_caret() -> Option<(String, f32)> {
    todo!()
}
