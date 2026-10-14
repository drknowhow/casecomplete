//! Low-level keyboard hook (WH_KEYBOARD_LL) on a dedicated thread.
//!
//! Contract (see docs/PLAN.md §2):
//! - The hook callback ONLY classifies the key, consults [`SwallowState`], and sends a
//!   [`HookEvent`] on the channel. It never blocks, never touches UIA/HTTP/overlay.
//! - Swallowing: Tab / Ctrl+Tab / Esc are swallowed only while `ghost_visible` is true.
//!   Left / Backspace are swallowed only while `now_ms < revert_until_ms` (then reported as
//!   `RevertRequested`). Ctrl+Alt+Z is always swallowed and reported as `RevertRequested`.
//! - Injected input (`LLKHF_INJECTED`) is ignored so our own SendInput never echoes back.
//! - `ShadowBuffer` is a pure data structure; the orchestrator (not the hook) updates it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    /// A printable character (already translated with current layout/shift state).
    Char(char),
    Backspace,
    Enter,
    Tab,
    CtrlTab,
    Esc,
    Left,
    Right,
    /// Any other navigation key (Home/End/Up/Down/PgUp/PgDn) — orchestrator resyncs.
    Nav,
    /// Left/Backspace inside the revert window, or Ctrl+Alt+Z.
    RevertRequested,
    /// Non-text key we do not model (Ctrl+C etc.). Carries the virtual-key code.
    Other(u32),
}

/// Shared with the hook thread; the orchestrator flips these.
#[derive(Debug, Default)]
pub struct SwallowState {
    pub ghost_visible: AtomicBool,
    /// Milliseconds since process start; Left/Backspace revert until then.
    pub revert_until_ms: AtomicU64,
}

impl SwallowState {
    pub fn set_ghost_visible(&self, v: bool) {
        self.ghost_visible.store(v, Ordering::SeqCst);
    }
    pub fn set_revert_until(&self, ms: u64) {
        self.revert_until_ms.store(ms, Ordering::SeqCst);
    }
}

/// Keeps the hook alive; dropping it unhooks and stops the thread.
pub struct HookHandle {
    _private: (),
}

/// Install the hook on a new thread. Events arrive on `tx`.
pub fn start(
    tx: crossbeam_channel::Sender<HookEvent>,
    swallow: Arc<SwallowState>,
) -> anyhow::Result<HookHandle> {
    let _ = (tx, swallow);
    todo!("cc-hook: implement WH_KEYBOARD_LL thread")
}

/// Milliseconds since process start (monotonic). Shared clock for the revert window.
pub fn now_ms() -> u64 {
    todo!()
}

/// Local model of the text typed into the focused field since the last resync.
#[derive(Debug, Default, Clone)]
pub struct ShadowBuffer {
    text: String,
}

impl ShadowBuffer {
    pub fn new() -> Self { Self::default() }
    pub fn reset(&mut self, text: &str) { self.text = text.to_string(); }
    pub fn push(&mut self, ch: char) { self.text.push(ch); }
    pub fn backspace(&mut self) { self.text.pop(); }
    pub fn text(&self) -> &str { &self.text }
    /// Characters after the last whitespace (the partial word being typed).
    pub fn current_word(&self) -> &str {
        match self.text.rfind(char::is_whitespace) {
            Some(i) => &self.text[i + 1..],
            None => &self.text,
        }
    }
    /// Text after the last sentence terminator (. ! ? or newline).
    pub fn last_sentence(&self) -> &str {
        match self.text.rfind(|c| matches!(c, '.' | '!' | '?' | '\n')) {
            Some(i) => self.text[i + 1..].trim_start(),
            None => &self.text,
        }
    }
    pub fn ends_with_word_boundary(&self) -> bool {
        self.text.chars().last().map_or(true, |c| c.is_whitespace() || c.is_ascii_punctuation())
    }
}
