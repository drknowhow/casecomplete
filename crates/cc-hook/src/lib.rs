//! Low-level keyboard hook (WH_KEYBOARD_LL) on a dedicated thread, plus a mouse hook that
//! only reports button presses (a click can move the caret behind the shadow buffer's back).
//!
//! Contract (see docs/PLAN.md §2):
//! - The hook callback ONLY classifies the key, consults [`SwallowState`], and sends a
//!   [`HookEvent`] on the channel. It never blocks, never touches UIA/HTTP/overlay.
//! - Swallowing: Tab / Ctrl+Tab / Esc are swallowed only while `ghost_visible` is true.
//!   Left / Backspace are swallowed only while `now_ms < revert_until_ms` (then reported as
//!   `RevertRequested`). Ctrl+Alt+Z is always swallowed and reported as `RevertRequested`.
//! - Injected input (`LLKHF_INJECTED`) is ignored so our own SendInput never echoes back.
//! - `ShadowBuffer` is a pure data structure; the orchestrator (not the hook) updates it.
//!
//! Implementation notes:
//! - Only key-down messages (`WM_KEYDOWN` / `WM_SYSKEYDOWN`) are examined; key-ups always
//!   pass through untouched.
//! - The swallow/event decision for non-text keys is the pure function [`decide`], unit
//!   tested without Windows. Text keys are translated with `ToUnicodeEx` using the
//!   foreground thread's keyboard layout and the live modifier state. `ToUnicodeEx` is called
//!   with the "do not change keyboard state" flag so the hook never disturbs the dead-key
//!   state of the application being typed into; dead keys pass through with no event.
//! - A panic inside the hook procedure is caught and treated as "pass the key through".

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::Context as _;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, GetKeyboardLayout, GetKeyboardState, ToUnicodeEx,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, PeekMessageW,
    PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_MBUTTONDOWN, WM_QUIT, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_USER,
};

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
    /// A text key swallowed while `SwallowState::capture` was set; the orchestrator must
    /// replay it after its own injection. '\u{8}' is Backspace, '\n' is Enter.
    Captured(char),
    /// A mouse button went down: the caret may have moved or another field taken focus.
    Click,
    /// Non-text key we do not model (Ctrl+C etc.). Carries the virtual-key code.
    Other(u32),
}

/// Shared with the hook thread; the orchestrator flips these.
#[derive(Debug, Default)]
pub struct SwallowState {
    pub ghost_visible: AtomicBool,
    /// Milliseconds since process start; Left/Backspace revert until then.
    pub revert_until_ms: AtomicU64,
    /// While set, text keys are swallowed and reported as `Captured` so they cannot
    /// interleave with an injection in progress.
    pub capture: AtomicBool,
    /// Keys reported as `Captured` so far. Counted before sending, so a key still on its
    /// way to the orchestrator shows up here first.
    pub captured: AtomicU64,
    /// Keys and clicks let through while `capture` was set (arrows, shortcuts, mouse
    /// buttons): each may have moved the caret or changed the text under an injection.
    pub passed: AtomicU64,
}

impl SwallowState {
    pub fn set_ghost_visible(&self, v: bool) {
        self.ghost_visible.store(v, Ordering::SeqCst);
    }
    pub fn set_revert_until(&self, ms: u64) {
        self.revert_until_ms.store(ms, Ordering::SeqCst);
    }
    pub fn set_capture(&self, v: bool) {
        self.capture.store(v, Ordering::SeqCst);
    }
    pub fn captured(&self) -> u64 {
        self.captured.load(Ordering::SeqCst)
    }
    pub fn passed(&self) -> u64 {
        self.passed.load(Ordering::SeqCst)
    }
}

/// Keeps the hook alive; dropping it unhooks and stops the thread.
pub struct HookHandle {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        // WM_QUIT makes the hook thread's GetMessage loop return, after which the thread
        // calls UnhookWindowsHookEx and exits. The thread's message queue is guaranteed to
        // exist because `start` only returns after the thread signalled readiness.
        // SAFETY: plain Win32 call with a thread id obtained from GetCurrentThreadId.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Err(e) = posted {
            log::debug!("cc-hook: PostThreadMessageW(WM_QUIT) failed (thread already gone?): {e}");
        }
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                log::warn!("cc-hook: hook thread panicked");
            }
        }
    }
}

/// Install the hook on a new thread. Events arrive on `tx`.
pub fn start(
    tx: crossbeam_channel::Sender<HookEvent>,
    swallow: Arc<SwallowState>,
) -> anyhow::Result<HookHandle> {
    // Pin the shared clock origin before the first key can arrive.
    let _ = now_ms();

    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
    let join = std::thread::Builder::new()
        .name("cc-hook".into())
        .spawn(move || hook_thread(tx, swallow, ready_tx))
        .context("failed to spawn cc-hook thread")?;

    match ready_rx.recv() {
        Ok(Ok(thread_id)) => Ok(HookHandle { thread_id, join: Some(join) }),
        Ok(Err(msg)) => {
            let _ = join.join();
            Err(anyhow::anyhow!(msg))
        }
        Err(_) => {
            let _ = join.join();
            Err(anyhow::anyhow!("cc-hook thread exited before installing the hook"))
        }
    }
}

/// Process-wide clock origin for [`now_ms`].
static CLOCK_START: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since process start (monotonic). Shared clock for the revert window.
pub fn now_ms() -> u64 {
    CLOCK_START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Local model of the text typed into the focused field since the last resync.
#[derive(Debug, Default, Clone)]
pub struct ShadowBuffer {
    text: String,
    /// The text starts where the field's text starts, so its first word is a whole word.
    /// False when the caret sits after text we have not seen.
    anchored: bool,
}

impl ShadowBuffer {
    pub fn new() -> Self { Self::default() }
    /// Everything before the caret, as read from the field.
    pub fn reset(&mut self, text: &str) {
        self.text = text.to_string();
        self.anchored = true;
    }
    /// The caret is somewhere in text we have not seen.
    pub fn reset_unknown(&mut self) {
        self.text.clear();
        self.anchored = false;
    }
    pub fn push(&mut self, ch: char) { self.text.push(ch); }
    pub fn backspace(&mut self) {
        if self.text.pop().is_none() {
            self.anchored = false;
        }
    }
    pub fn text(&self) -> &str { &self.text }
    pub fn anchored(&self) -> bool { self.anchored }
    /// Characters after the last whitespace (the partial word being typed).
    pub fn current_word(&self) -> &str {
        // `char_indices` + `len_utf8` so multi-byte whitespace (e.g. U+00A0) cannot split a char.
        match self.text.char_indices().rev().find(|(_, c)| c.is_whitespace()) {
            Some((i, c)) => &self.text[i + c.len_utf8()..],
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

// ---------------------------------------------------------------------------------------
// Pure decision logic (no Windows calls)
// ---------------------------------------------------------------------------------------

/// Virtual-key codes used by the pure logic (plain `u32` so [`decide`] is platform-free).
mod vk {
    pub const BACK: u32 = 0x08;
    pub const TAB: u32 = 0x09;
    pub const RETURN: u32 = 0x0D;
    pub const SHIFT: u32 = 0x10;
    pub const CONTROL: u32 = 0x11;
    pub const MENU: u32 = 0x12;
    pub const CAPITAL: u32 = 0x14;
    pub const ESCAPE: u32 = 0x1B;
    pub const PRIOR: u32 = 0x21;
    pub const NEXT: u32 = 0x22;
    pub const END: u32 = 0x23;
    pub const HOME: u32 = 0x24;
    pub const LEFT: u32 = 0x25;
    pub const UP: u32 = 0x26;
    pub const RIGHT: u32 = 0x27;
    pub const DOWN: u32 = 0x28;
    pub const Z: u32 = 0x5A;
    pub const LWIN: u32 = 0x5B;
    pub const RWIN: u32 = 0x5C;
    pub const LSHIFT: u32 = 0xA0;
    pub const RSHIFT: u32 = 0xA1;
    pub const LCONTROL: u32 = 0xA2;
    pub const RCONTROL: u32 = 0xA3;
    pub const PACKET: u32 = 0xE7;
    pub const LMENU: u32 = 0xA4;
    pub const RMENU: u32 = 0xA5;
}

/// Classify a key-down for a NON-text interpretation and decide whether to swallow it.
///
/// Returns `(event, swallow)`. Printable keys are not recognised here: they come back as
/// `Other(vk)` and the hook then upgrades them to `Char` via keyboard-layout translation.
///
/// Rules:
/// - Ctrl+Alt+Z: always `RevertRequested`, swallowed.
/// - Any other Alt combination (Alt+Tab, Alt+Left, Alt+F4 ...): `Other(vk)`, never swallowed.
/// - Ctrl+Tab: `CtrlTab`, swallowed only while the ghost is visible.
/// - Ctrl+Left / Right / Backspace / Home / End / Up / Down / PgUp / PgDn move the caret or
///   delete by word, which the shadow buffer cannot model: `Nav`, never swallowed.
///   Every other Ctrl combination is `Other(vk)`, never swallowed.
/// - Tab / Esc: swallowed only while the ghost is visible.
/// - Left / Backspace: inside the revert window they are swallowed and reported as
///   `RevertRequested`; otherwise plain `Left` / `Backspace`, passed through.
pub(crate) fn decide(
    vk: u32,
    ctrl: bool,
    alt: bool,
    ghost_visible: bool,
    in_revert_window: bool,
) -> (Option<HookEvent>, bool) {
    use HookEvent as E;

    if ctrl && alt && vk == vk::Z {
        return (Some(E::RevertRequested), true);
    }
    if alt {
        return (Some(E::Other(vk)), false);
    }
    if ctrl {
        return match vk {
            vk::TAB => (Some(E::CtrlTab), ghost_visible),
            vk::BACK | vk::LEFT | vk::RIGHT | vk::HOME | vk::END | vk::UP | vk::DOWN
            | vk::PRIOR | vk::NEXT => (Some(E::Nav), false),
            _ => (Some(E::Other(vk)), false),
        };
    }
    match vk {
        vk::BACK if in_revert_window => (Some(E::RevertRequested), true),
        vk::BACK => (Some(E::Backspace), false),
        vk::LEFT if in_revert_window => (Some(E::RevertRequested), true),
        vk::LEFT => (Some(E::Left), false),
        vk::RIGHT => (Some(E::Right), false),
        vk::RETURN => (Some(E::Enter), false),
        vk::TAB => (Some(E::Tab), ghost_visible),
        vk::ESCAPE => (Some(E::Esc), ghost_visible),
        vk::HOME | vk::END | vk::UP | vk::DOWN | vk::PRIOR | vk::NEXT => (Some(E::Nav), false),
        _ => (Some(E::Other(vk)), false),
    }
}

/// Whether a key press with this modifier state can produce text. Plain and Shift keys can;
/// Ctrl+x / Alt+x / Win+x are shortcuts. Ctrl+Alt together is how Windows reports AltGr, so
/// that combination is allowed to produce text.
fn may_translate(ctrl: bool, alt: bool, win: bool) -> bool {
    !win && ctrl == alt
}

// ---------------------------------------------------------------------------------------
// Windows plumbing
// ---------------------------------------------------------------------------------------

/// `dwExtraInfo` tag ("CCOM") used by our own SendInput calls.
const OWN_EXTRA_INFO: usize = 0x4343_4F4D;

/// `ToUnicodeEx` wFlags bit 2: do not change the keyboard (dead-key) state (Windows 10 1607+).
const TOUNICODE_NO_STATE_CHANGE: u32 = 0x4;

struct HookCtx {
    tx: crossbeam_channel::Sender<HookEvent>,
    swallow: Arc<SwallowState>,
}

thread_local! {
    /// Low-level hook procedures run on the thread that installed the hook, so per-thread
    /// state needs no locking.
    static CTX: RefCell<Option<HookCtx>> = const { RefCell::new(None) };
}

fn hook_thread(
    tx: crossbeam_channel::Sender<HookEvent>,
    swallow: Arc<SwallowState>,
    ready: mpsc::Sender<Result<u32, String>>,
) {
    CTX.with(|c| *c.borrow_mut() = Some(HookCtx { tx, swallow }));

    // SAFETY: straightforward Win32 calls on the current thread; `msg` outlives the loop.
    unsafe {
        let mut msg = MSG::default();
        // Force creation of this thread's message queue so PostThreadMessageW cannot fail
        // with ERROR_INVALID_THREAD_ID once `start` has returned.
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        // Windows silently drops a low-level hook that answers too slowly; keep this thread
        // responsive under load.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);

        let hinst = match GetModuleHandleW(None) {
            Ok(h) => HINSTANCE(h.0),
            Err(e) => {
                let _ = ready.send(Err(format!("GetModuleHandleW failed: {e}")));
                return;
            }
        };
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hinst, 0) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(format!("SetWindowsHookExW(WH_KEYBOARD_LL) failed: {e}")));
                return;
            }
        };
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0)
            .inspect_err(|e| log::warn!("cc-hook: mouse hook not installed, clicks go unnoticed: {e}"))
            .ok();
        let _ = ready.send(Ok(GetCurrentThreadId()));

        // The hook procedures are invoked from inside GetMessageW. 0 = WM_QUIT, -1 = error.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {}

        if let Err(e) = UnhookWindowsHookEx(hook) {
            log::warn!("cc-hook: UnhookWindowsHookEx failed: {e}");
        }
        if let Some(mouse) = mouse {
            let _ = UnhookWindowsHookEx(mouse);
        }
    }
    CTX.with(|c| *c.borrow_mut() = None);
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lParam points to a KBDLLHOOKSTRUCT that
        // is valid for the duration of this call.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let swallow =
            catch_unwind(AssertUnwindSafe(|| handle_key(wparam.0 as u32, kb))).unwrap_or(false);
        if swallow {
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding the unchanged arguments; no locks or borrows are held here.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Reports button presses only; moves and wheel turns pass untouched.
unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && matches!(wparam.0 as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
        CTX.with(|cell| {
            if let Ok(guard) = cell.try_borrow() {
                if let Some(ctx) = guard.as_ref() {
                    if ctx.swallow.capture.load(Ordering::SeqCst) {
                        ctx.swallow.passed.fetch_add(1, Ordering::SeqCst);
                    }
                    let _ = ctx.tx.try_send(HookEvent::Click);
                }
            }
        });
    }
    // SAFETY: forwarding the unchanged arguments; no locks or borrows are held here.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Sends a typed character: as `Captured` while capturing, else as `Char`.
fn send_char(ctx: &HookCtx, c: char, capture: bool) {
    if capture {
        ctx.swallow.captured.fetch_add(1, Ordering::SeqCst);
        let _ = ctx.tx.try_send(HookEvent::Captured(c));
    } else {
        let _ = ctx.tx.try_send(HookEvent::Char(c));
    }
}

fn is_modifier(vk: u32) -> bool {
    use vk::*;
    matches!(
        vk,
        SHIFT | LSHIFT | RSHIFT | CONTROL | LCONTROL | RCONTROL | MENU | LMENU | RMENU | LWIN | RWIN | CAPITAL
    )
}

fn accept_injected() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CC_ACCEPT_INJECTED").is_some_and(|v| v == "1"))
}

/// Returns `true` when the key must be swallowed.
fn handle_key(msg: u32, kb: &KBDLLHOOKSTRUCT) -> bool {
    if msg != WM_KEYDOWN && msg != WM_SYSKEYDOWN {
        return false;
    }
    if kb.dwExtraInfo == OWN_EXTRA_INFO {
        log::trace!("own vk={:#x} scan={:#x} flags={:#x}", kb.vkCode, kb.scanCode, kb.flags.0);
        return false;
    }
    // Synthetic input from other processes is ignored unless the e2e flag is set
    // (CC_ACCEPT_INJECTED=1 lets a test harness drive the app with SendKeys).
    if kb.flags.0 & LLKHF_INJECTED.0 != 0 && !accept_injected() {
        return false;
    }

    CTX.with(|cell| {
        let Ok(guard) = cell.try_borrow() else { return false };
        let Some(ctx) = guard.as_ref() else { return false };

        let vk = kb.vkCode;
        let ctrl = key_down(vk::CONTROL);
        let alt = key_down(vk::MENU);
        let win = key_down(vk::LWIN) || key_down(vk::RWIN);
        let ghost = ctx.swallow.ghost_visible.load(Ordering::SeqCst);
        let in_revert = now_ms() < ctx.swallow.revert_until_ms.load(Ordering::SeqCst);

        let (ev, swallow) = decide(vk, ctrl, alt, ghost, in_revert);
        let capture = ctx.swallow.capture.load(Ordering::SeqCst);
        match ev {
            Some(HookEvent::Backspace) if capture => {
                send_char(ctx, '\u{8}', true);
                return true;
            }
            Some(HookEvent::Enter) if capture => {
                send_char(ctx, '\n', true);
                return true;
            }
            _ => {}
        }
        let passes = |ctx: &HookCtx| {
            if capture && !is_modifier(vk) {
                ctx.swallow.passed.fetch_add(1, Ordering::SeqCst);
            }
        };

        // Text keys (decide() says Other), plus Ctrl+Alt+Z, which on AltGr layouts can be a
        // real character (e.g. Polish AltGr+Z) and must not be eaten as a revert.
        let altgr_z = vk == vk::Z && ctrl && alt;
        let try_text =
            !win && ((matches!(ev, Some(HookEvent::Other(_))) && may_translate(ctrl, alt, win)) || altgr_z);
        if try_text {
            // KEYEVENTF_UNICODE arrives as VK_PACKET with the UTF-16 unit in scanCode.
            // Calling ToUnicodeEx on it consumes the pending character and corrupts what
            // the target app receives, so read the unit directly instead.
            if vk == vk::PACKET {
                if let Some(c) = char::from_u32(kb.scanCode) {
                    send_char(ctx, c, capture);
                }
                return capture;
            }
            match translate(vk, kb.scanCode) {
                Translation::Text(buf) => {
                    for &c in &buf.chars[..buf.len] {
                        send_char(ctx, c, capture);
                    }
                    return capture;
                }
                // Dead key: let the app compose it; the orchestrator sees nothing.
                Translation::Dead => {
                    passes(ctx);
                    return false;
                }
                Translation::NoText => {}
            }
        }

        if let Some(e) = ev {
            // Drop on full: the hook must never block.
            let _ = ctx.tx.try_send(e);
        }
        if !swallow {
            passes(ctx);
        }
        swallow
    })
}

/// High bit of GetAsyncKeyState = key currently down.
fn key_down(vk: u32) -> bool {
    // SAFETY: GetAsyncKeyState has no preconditions.
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

struct CharBuf {
    chars: [char; 4],
    len: usize,
}

enum Translation {
    Text(CharBuf),
    /// Dead key (accent waiting for a base letter).
    Dead,
    /// The key produces no printable text (function keys, modifiers, control codes ...).
    NoText,
}

/// Translate a key-down to text with the foreground thread's layout and the live modifier
/// state. Allocation-free.
fn translate(vk: u32, scan: u32) -> Translation {
    const MODIFIERS: [u32; 9] = [
        vk::SHIFT,
        vk::LSHIFT,
        vk::RSHIFT,
        vk::CONTROL,
        vk::LCONTROL,
        vk::RCONTROL,
        vk::MENU,
        vk::LMENU,
        vk::RMENU,
    ];

    let mut state = [0u8; 256];
    let mut wbuf = [0u16; 8];
    // SAFETY: all buffers are valid for the sizes the APIs expect.
    let n = unsafe {
        // Base: toggle keys (NumLock/ScrollLock). Its modifier bytes can be stale on a hook
        // thread, so overlay them from the real async state; CapsLock's toggle bit comes
        // from GetKeyState.
        let _ = GetKeyboardState(&mut state);
        for k in MODIFIERS {
            if let Some(slot) = state.get_mut(k as usize) {
                *slot = if key_down(k) { 0x80 } else { 0 };
            }
        }
        if let Some(slot) = state.get_mut(vk::CAPITAL as usize) {
            *slot = (GetKeyState(vk::CAPITAL as i32) & 1) as u8;
        }

        let fg = GetForegroundWindow();
        let tid = if fg.is_invalid() { 0 } else { GetWindowThreadProcessId(fg, None) };
        let hkl = GetKeyboardLayout(tid);

        ToUnicodeEx(vk, scan, &state, &mut wbuf, TOUNICODE_NO_STATE_CHANGE, hkl)
    };

    if n < 0 {
        return Translation::Dead;
    }
    let n = (n as usize).min(wbuf.len());
    let mut out = CharBuf { chars: ['\0'; 4], len: 0 };
    for c in char::decode_utf16(wbuf[..n].iter().copied()).filter_map(Result::ok) {
        // Control codes (Ctrl+letter, Esc, Backspace ...) are keys, not text.
        if c.is_control() || out.len == out.chars.len() {
            continue;
        }
        out.chars[out.len] = c;
        out.len += 1;
    }
    if out.len == 0 {
        Translation::NoText
    } else {
        Translation::Text(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HookEvent as E;

    // ---- decide ----

    fn d(vk: u32, ctrl: bool, alt: bool, ghost: bool, revert: bool) -> (Option<E>, bool) {
        decide(vk, ctrl, alt, ghost, revert)
    }

    #[test]
    fn tab_swallowed_only_when_ghost_visible() {
        assert_eq!(d(vk::TAB, false, false, true, false), (Some(E::Tab), true));
        assert_eq!(d(vk::TAB, false, false, false, false), (Some(E::Tab), false));
    }

    #[test]
    fn ctrl_tab_swallowed_only_when_ghost_visible() {
        assert_eq!(d(vk::TAB, true, false, true, false), (Some(E::CtrlTab), true));
        assert_eq!(d(vk::TAB, true, false, false, false), (Some(E::CtrlTab), false));
    }

    #[test]
    fn esc_swallowed_only_when_ghost_visible() {
        assert_eq!(d(vk::ESCAPE, false, false, true, false), (Some(E::Esc), true));
        assert_eq!(d(vk::ESCAPE, false, false, false, false), (Some(E::Esc), false));
    }

    #[test]
    fn left_and_backspace_swallowed_only_in_revert_window() {
        assert_eq!(d(vk::LEFT, false, false, false, true), (Some(E::RevertRequested), true));
        assert_eq!(d(vk::BACK, false, false, false, true), (Some(E::RevertRequested), true));
        assert_eq!(d(vk::LEFT, false, false, false, false), (Some(E::Left), false));
        assert_eq!(d(vk::BACK, false, false, false, false), (Some(E::Backspace), false));
    }

    #[test]
    fn ghost_does_not_swallow_left_or_backspace_and_revert_does_not_swallow_tab() {
        assert_eq!(d(vk::LEFT, false, false, true, false), (Some(E::Left), false));
        assert_eq!(d(vk::BACK, false, false, true, false), (Some(E::Backspace), false));
        assert_eq!(d(vk::TAB, false, false, false, true), (Some(E::Tab), false));
        assert_eq!(d(vk::ESCAPE, false, false, false, true), (Some(E::Esc), false));
    }

    #[test]
    fn ghost_and_revert_together() {
        assert_eq!(d(vk::TAB, false, false, true, true), (Some(E::Tab), true));
        assert_eq!(d(vk::LEFT, false, false, true, true), (Some(E::RevertRequested), true));
    }

    #[test]
    fn ctrl_alt_z_always_swallowed() {
        for ghost in [false, true] {
            for revert in [false, true] {
                assert_eq!(
                    d(vk::Z, true, true, ghost, revert),
                    (Some(E::RevertRequested), true),
                    "ghost={ghost} revert={revert}"
                );
            }
        }
    }

    #[test]
    fn z_with_only_one_of_ctrl_alt_is_not_a_revert() {
        assert_eq!(d(vk::Z, true, false, false, false), (Some(E::Other(vk::Z)), false));
        assert_eq!(d(vk::Z, false, true, false, false), (Some(E::Other(vk::Z)), false));
        assert_eq!(d(vk::Z, false, false, false, false), (Some(E::Other(vk::Z)), false));
    }

    #[test]
    fn enter_and_right_never_swallowed() {
        for ghost in [false, true] {
            for revert in [false, true] {
                assert_eq!(d(vk::RETURN, false, false, ghost, revert), (Some(E::Enter), false));
                assert_eq!(d(vk::RIGHT, false, false, ghost, revert), (Some(E::Right), false));
            }
        }
    }

    #[test]
    fn nav_keys_map_to_nav_and_pass_through() {
        for k in [vk::HOME, vk::END, vk::UP, vk::DOWN, vk::PRIOR, vk::NEXT] {
            assert_eq!(d(k, false, false, true, true), (Some(E::Nav), false), "vk={k:#x}");
        }
    }

    #[test]
    fn unmodelled_keys_are_other() {
        // F5, Delete, 'A'
        for k in [0x74u32, 0x2E, 0x41] {
            assert_eq!(d(k, false, false, true, true), (Some(E::Other(k)), false));
        }
    }

    #[test]
    fn alt_combinations_are_never_swallowed() {
        // Alt+Tab, Alt+Esc, Alt+Left, Alt+Backspace must keep working with ghost/revert active.
        for k in [vk::TAB, vk::ESCAPE, vk::LEFT, vk::BACK] {
            assert_eq!(d(k, false, true, true, true), (Some(E::Other(k)), false), "vk={k:#x}");
        }
    }

    #[test]
    fn ctrl_caret_moves_are_nav_and_never_swallowed() {
        for k in [vk::LEFT, vk::RIGHT, vk::BACK, vk::HOME, vk::END] {
            assert_eq!(d(k, true, false, true, true), (Some(E::Nav), false), "vk={k:#x}");
        }
        // Ctrl+Esc (Start menu) and Ctrl+C are not swallowed either.
        assert_eq!(d(vk::ESCAPE, true, false, true, true), (Some(E::Other(vk::ESCAPE)), false));
        assert_eq!(d(0x43, true, false, true, true), (Some(E::Other(0x43)), false));
    }

    #[test]
    fn may_translate_matrix() {
        assert!(may_translate(false, false, false)); // plain / Shift
        assert!(may_translate(true, true, false)); // AltGr
        assert!(!may_translate(true, false, false)); // Ctrl+x
        assert!(!may_translate(false, true, false)); // Alt+x
        assert!(!may_translate(false, false, true)); // Win+x
        assert!(!may_translate(true, true, true));
    }

    // ---- now_ms ----

    #[test]
    fn now_ms_is_monotonic() {
        let a = now_ms();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let b = now_ms();
        assert!(b >= a + 10, "a={a} b={b}");
    }

    // ---- ShadowBuffer ----

    fn buf(s: &str) -> ShadowBuffer {
        let mut b = ShadowBuffer::new();
        b.reset(s);
        b
    }

    #[test]
    fn push_backspace_reset_text() {
        let mut b = ShadowBuffer::new();
        assert_eq!(b.text(), "");
        b.push('h');
        b.push('i');
        assert_eq!(b.text(), "hi");
        b.backspace();
        assert_eq!(b.text(), "h");
        b.backspace();
        b.backspace(); // no-op on empty
        assert_eq!(b.text(), "");
        b.reset("abc");
        assert_eq!(b.text(), "abc");
        b.push('é');
        b.backspace();
        assert_eq!(b.text(), "abc");
    }

    #[test]
    fn anchoring_follows_what_is_known_before_the_caret() {
        let mut b = ShadowBuffer::new();
        assert!(!b.anchored());
        b.reset("");
        assert!(b.anchored());
        b.push('a');
        b.backspace();
        assert!(b.anchored());
        b.backspace();
        assert!(!b.anchored(), "deleting past the start reaches unseen text");
        b.reset("some text");
        assert!(b.anchored());
        b.reset_unknown();
        assert_eq!((b.text(), b.anchored()), ("", false));
    }

    #[test]
    fn backspace_removes_one_whole_char() {
        let mut b = buf("ab😀");
        b.backspace();
        assert_eq!(b.text(), "ab");
    }

    #[test]
    fn current_word_cases() {
        assert_eq!(buf("").current_word(), "");
        assert_eq!(buf("hello").current_word(), "hello");
        assert_eq!(buf("hello wor").current_word(), "wor");
        assert_eq!(buf("hello ").current_word(), "");
        assert_eq!(buf("a\tb\nc d").current_word(), "d");
        assert_eq!(buf("foo  bar").current_word(), "bar");
    }

    #[test]
    fn current_word_handles_multibyte_whitespace_and_text() {
        // U+00A0 (2 bytes) and U+3000 (3 bytes) are whitespace per char::is_whitespace.
        assert_eq!(buf("foo\u{a0}bar").current_word(), "bar");
        assert_eq!(buf("foo\u{3000}ba").current_word(), "ba");
        assert_eq!(buf("naïve café").current_word(), "café");
    }

    #[test]
    fn last_sentence_cases() {
        assert_eq!(buf("").last_sentence(), "");
        assert_eq!(buf("no terminator").last_sentence(), "no terminator");
        assert_eq!(buf("First one. Second one").last_sentence(), "Second one");
        assert_eq!(buf("Done! next").last_sentence(), "next");
        assert_eq!(buf("Really? yes").last_sentence(), "yes");
        assert_eq!(buf("line one\nline two").last_sentence(), "line two");
        assert_eq!(buf("ends with period.").last_sentence(), "");
        assert_eq!(buf("A. B. C").last_sentence(), "C");
        assert_eq!(buf("Hi. ünï").last_sentence(), "ünï");
    }

    #[test]
    fn ends_with_word_boundary_cases() {
        assert!(buf("").ends_with_word_boundary());
        assert!(buf("hello ").ends_with_word_boundary());
        assert!(buf("hello\n").ends_with_word_boundary());
        assert!(buf("hello,").ends_with_word_boundary());
        assert!(buf("hello.").ends_with_word_boundary());
        assert!(!buf("hello").ends_with_word_boundary());
        assert!(!buf("h").ends_with_word_boundary());
        assert!(!buf("café").ends_with_word_boundary());
    }

    // ---- hook_proc plumbing (called directly on the test thread; needs no real hook) ----

    fn install_ctx() -> (crossbeam_channel::Receiver<HookEvent>, Arc<SwallowState>) {
        let (tx, rx) = crossbeam_channel::bounded(16);
        let swallow = Arc::new(SwallowState::default());
        CTX.with(|c| *c.borrow_mut() = Some(HookCtx { tx, swallow: swallow.clone() }));
        (rx, swallow)
    }

    fn kb(vk: u32, flags: u32, extra: usize) -> KBDLLHOOKSTRUCT {
        use windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT_FLAGS;
        KBDLLHOOKSTRUCT {
            vkCode: vk,
            scanCode: 0,
            flags: KBDLLHOOKSTRUCT_FLAGS(flags),
            time: 0,
            dwExtraInfo: extra,
        }
    }

    /// Returns whether the key was swallowed.
    fn feed(msg: u32, k: &KBDLLHOOKSTRUCT) -> bool {
        let r = unsafe {
            hook_proc(HC_ACTION as i32, WPARAM(msg as usize), LPARAM(k as *const _ as isize))
        };
        r.0 == 1
    }

    // These assume no Ctrl/Alt/Win key is physically held while the tests run.

    #[test]
    fn proc_ghost_tab_swallowed_and_reported() {
        let (rx, swallow) = install_ctx();
        let k = kb(vk::TAB, 0, 0);
        assert!(!feed(WM_KEYDOWN, &k));
        assert_eq!(rx.try_recv(), Ok(E::Tab));
        swallow.set_ghost_visible(true);
        assert!(feed(WM_KEYDOWN, &k));
        assert_eq!(rx.try_recv(), Ok(E::Tab));
        assert!(feed(WM_SYSKEYDOWN, &k));
        assert_eq!(rx.try_recv(), Ok(E::Tab));
    }

    #[test]
    fn proc_revert_window_swallows_left_and_backspace() {
        let (rx, swallow) = install_ctx();
        let left = kb(vk::LEFT, 0, 0);
        let back = kb(vk::BACK, 0, 0);
        swallow.set_revert_until(now_ms() + 60_000);
        assert!(feed(WM_KEYDOWN, &left));
        assert_eq!(rx.try_recv(), Ok(E::RevertRequested));
        assert!(feed(WM_KEYDOWN, &back));
        assert_eq!(rx.try_recv(), Ok(E::RevertRequested));
        swallow.set_revert_until(0);
        assert!(!feed(WM_KEYDOWN, &left));
        assert_eq!(rx.try_recv(), Ok(E::Left));
        assert!(!feed(WM_KEYDOWN, &back));
        assert_eq!(rx.try_recv(), Ok(E::Backspace));
    }

    #[test]
    fn proc_counts_captured_keys_and_keys_let_through_while_capturing() {
        let (rx, swallow) = install_ctx();
        swallow.set_capture(true);
        assert!(feed(WM_KEYDOWN, &kb(vk::BACK, 0, 0)));
        assert_eq!(rx.try_recv(), Ok(E::Captured('\u{8}')));
        assert_eq!(swallow.captured(), 1);
        assert!(!feed(WM_KEYDOWN, &kb(vk::DOWN, 0, 0)));
        assert_eq!(rx.try_recv(), Ok(E::Nav));
        assert!(!feed(WM_KEYDOWN, &kb(vk::SHIFT, 0, 0)));
        assert_eq!(swallow.passed(), 1, "a modifier alone changes nothing");
        swallow.set_capture(false);
        assert!(!feed(WM_KEYDOWN, &kb(vk::DOWN, 0, 0)));
        assert_eq!((swallow.captured(), swallow.passed()), (1, 1));
    }

    #[test]
    fn mouse_proc_reports_button_presses_only() {
        const WM_MOUSEMOVE: u32 = 0x0200;
        let (rx, swallow) = install_ctx();
        let click = |msg: u32| unsafe { mouse_proc(HC_ACTION as i32, WPARAM(msg as usize), LPARAM(0)) };
        click(WM_MOUSEMOVE);
        assert!(rx.try_recv().is_err());
        click(WM_LBUTTONDOWN);
        assert_eq!(rx.try_recv(), Ok(E::Click));
        assert_eq!(swallow.passed(), 0);
        swallow.set_capture(true);
        click(WM_RBUTTONDOWN);
        assert_eq!(rx.try_recv(), Ok(E::Click));
        assert_eq!(swallow.passed(), 1);
    }

    #[test]
    fn proc_ignores_injected_own_tag_and_key_up() {
        let (rx, swallow) = install_ctx();
        swallow.set_ghost_visible(true);
        let injected = kb(vk::TAB, LLKHF_INJECTED.0, 0);
        assert!(!feed(WM_KEYDOWN, &injected));
        let tagged = kb(vk::TAB, 0, OWN_EXTRA_INFO);
        assert!(!feed(WM_KEYDOWN, &tagged));
        let plain = kb(vk::TAB, 0, 0);
        const WM_KEYUP: u32 = 0x0101;
        assert!(!feed(WM_KEYUP, &plain));
        assert!(rx.try_recv().is_err(), "no events expected");
    }

    #[test]
    fn proc_full_channel_drops_instead_of_blocking() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let swallow = Arc::new(SwallowState::default());
        CTX.with(|c| *c.borrow_mut() = Some(HookCtx { tx, swallow }));
        let k = kb(vk::RETURN, 0, 0);
        for _ in 0..5 {
            assert!(!feed(WM_KEYDOWN, &k));
        }
        assert_eq!(rx.try_recv(), Ok(E::Enter));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn proc_without_ctx_passes_through() {
        CTX.with(|c| *c.borrow_mut() = None);
        let k = kb(vk::TAB, 0, 0);
        assert!(!feed(WM_KEYDOWN, &k));
    }

    #[test]
    fn proc_negative_code_passes_through() {
        let (rx, swallow) = install_ctx();
        swallow.set_ghost_visible(true);
        let k = kb(vk::TAB, 0, 0);
        let r = unsafe { hook_proc(-1, WPARAM(WM_KEYDOWN as usize), LPARAM(&k as *const _ as isize)) };
        assert_ne!(r.0, 1);
        assert!(rx.try_recv().is_err());
    }

    // ---- real hook install / teardown ----

    #[test]
    fn start_and_drop_installs_and_removes_hook() {
        let (tx, _rx) = crossbeam_channel::bounded(16);
        let handle = start(tx, Arc::new(SwallowState::default())).expect("hook should install");
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(handle); // joins the hook thread; must not hang
    }

    #[test]
    fn can_start_again_after_drop() {
        for _ in 0..3 {
            let (tx, _rx) = crossbeam_channel::bounded(16);
            let h = start(tx, Arc::new(SwallowState::default())).expect("hook should install");
            drop(h);
        }
    }
}
