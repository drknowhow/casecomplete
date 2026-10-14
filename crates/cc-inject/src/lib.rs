//! Text injection via SendInput. Called from a worker thread, never the hook thread.
//!
//! Contract:
//! - `type_text`: KEYEVENTF_UNICODE key down/up pairs, batched in one SendInput call per
//!   ~32 chars. '\n' is sent as VK_RETURN. Surrogate pairs sent as two UNICODE events.
//! - `backspace(n)`: n VK_BACK presses in one SendInput batch.
//! - `press(vk, ctrl, shift, alt)`: a single chord.
//! - `select_back(n)`: n x Shift+Left (used by the select-and-retype strategy).
//! - All injected events are tagged with `dwExtraInfo = INJECT_TAG` so cc-hook can
//!   recognise them (in addition to LLKHF_INJECTED).

pub const INJECT_TAG: usize = 0x4343_4F4D; // "CCOM"

pub fn type_text(text: &str) -> anyhow::Result<()> {
    let _ = text;
    todo!("cc-inject: SendInput unicode")
}

pub fn backspace(n: usize) -> anyhow::Result<()> {
    let _ = n;
    todo!()
}

pub fn select_back(n: usize) -> anyhow::Result<()> {
    let _ = n;
    todo!()
}

pub fn press(vk: u16, ctrl: bool, shift: bool, alt: bool) -> anyhow::Result<()> {
    let _ = (vk, ctrl, shift, alt);
    todo!()
}
