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

use anyhow::{bail, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
    VK_HOME, VK_INSERT, VK_LEFT, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_UP,
    VK_V,
};

pub const INJECT_TAG: usize = 0x4343_4F4D; // "CCOM"

/// Maximum number of INPUT records passed to one SendInput call.
const MAX_BATCH: usize = 64;

pub fn type_text(text: &str) -> anyhow::Result<()> {
    send(&unicode_inputs(text))
}

pub fn backspace(n: usize) -> anyhow::Result<()> {
    send(&backspace_inputs(n))
}

pub fn select_back(n: usize) -> anyhow::Result<()> {
    send(&select_back_inputs(n))
}

pub fn press(vk: u16, ctrl: bool, shift: bool, alt: bool) -> anyhow::Result<()> {
    send(&press_inputs(vk, ctrl, shift, alt))
}

/// Inserts `text` by placing it on the clipboard and sending Ctrl+V, then restores the
/// previous clipboard text. Slower than `type_text` but immune to apps that garble long
/// bursts of KEYEVENTF_UNICODE input.
pub fn paste_text(text: &str) -> anyhow::Result<()> {
    let previous = clipboard::get_text();
    clipboard::set_text(text)?;
    press(VK_V.0, true, false, false)?;
    std::thread::sleep(std::time::Duration::from_millis(150));
    if let Some(prev) = previous {
        let _ = clipboard::set_text(&prev);
    }
    Ok(())
}

mod clipboard {
    use anyhow::{bail, Context, Result};
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    struct Open;

    impl Open {
        fn new() -> Result<Open> {
            for _ in 0..10 {
                // SAFETY: plain Win32 call; failure means another app holds the clipboard.
                if unsafe { OpenClipboard(HWND::default()) }.is_ok() {
                    return Ok(Open);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            bail!("clipboard is busy")
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: paired with the successful OpenClipboard in `new`.
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    pub fn get_text() -> Option<String> {
        let _open = Open::new().ok()?;
        // SAFETY: the clipboard is open on this thread; the handle stays valid until close.
        unsafe {
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut len = 0;
            while *p.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            let _ = GlobalUnlock(HGLOBAL(h.0));
            Some(s)
        }
    }

    pub fn set_text(text: &str) -> Result<()> {
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let _open = Open::new()?;
        // SAFETY: the buffer is allocated for exactly `units` and ownership passes to the
        // clipboard on a successful SetClipboardData.
        unsafe {
            EmptyClipboard().context("EmptyClipboard")?;
            let h = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2).context("GlobalAlloc")?;
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                bail!("GlobalLock failed");
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
            let _ = GlobalUnlock(h);
            SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(h.0)).context("SetClipboardData")?;
        }
        Ok(())
    }
}

/// Sends records in batches of at most `MAX_BATCH`. Shift held by `select_back` stays
/// held across chunks because the down and up records are simply placed in the first
/// and last chunk.
fn send(inputs: &[INPUT]) -> Result<()> {
    for chunk in inputs.chunks(MAX_BATCH) {
        // SAFETY: the slice is valid for `chunk.len()` INPUT records and the size matches
        // the INPUT type.
        let sent = unsafe { SendInput(chunk, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != chunk.len() {
            bail!(
                "SendInput injected {} of {} records (os error: {})",
                sent,
                chunk.len(),
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

/// A virtual-key down or up record. Extended keys get KEYEVENTF_EXTENDEDKEY.
fn vk_event(vk: u16, up: bool, extended: bool) -> INPUT {
    let mut flags = 0u32;
    if up {
        flags |= KEYEVENTF_KEYUP.0;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY.0;
    }
    key_input(vk, 0, flags)
}

/// A KEYEVENTF_UNICODE record carrying one UTF-16 code unit.
fn unicode_event(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE.0;
    if up {
        flags |= KEYEVENTF_KEYUP.0;
    }
    key_input(0, unit, flags)
}

/// Keys that Windows treats as extended (navigation cluster and arrows).
fn is_extended(vk: u16) -> bool {
    [
        VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN, VK_HOME, VK_END, VK_PRIOR, VK_NEXT, VK_INSERT,
        VK_DELETE,
    ]
    .iter()
    .any(|k| k.0 == vk)
}

/// Pure record builder for `type_text`.
pub(crate) fn unicode_inputs(text: &str) -> Vec<INPUT> {
    let mut out = Vec::with_capacity(text.len() * 2);
    let mut buf = [0u16; 2];
    for ch in text.chars() {
        match ch {
            '\r' => {}
            '\n' => {
                out.push(vk_event(VK_RETURN.0, false, false));
                out.push(vk_event(VK_RETURN.0, true, false));
            }
            _ => {
                // A char outside the BMP encodes to two units (surrogate pair), each sent
                // as its own UNICODE down/up pair.
                for unit in ch.encode_utf16(&mut buf).iter() {
                    out.push(unicode_event(*unit, false));
                    out.push(unicode_event(*unit, true));
                }
            }
        }
    }
    out
}

/// Pure record builder for `backspace`.
pub(crate) fn backspace_inputs(n: usize) -> Vec<INPUT> {
    let mut out = Vec::with_capacity(n * 2);
    for _ in 0..n {
        out.push(vk_event(VK_BACK.0, false, false));
        out.push(vk_event(VK_BACK.0, true, false));
    }
    out
}

/// Pure record builder for `select_back`: Shift down, n x Left, Shift up.
pub(crate) fn select_back_inputs(n: usize) -> Vec<INPUT> {
    if n == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(n * 2 + 2);
    out.push(vk_event(VK_SHIFT.0, false, false));
    for _ in 0..n {
        out.push(vk_event(VK_LEFT.0, false, true));
        out.push(vk_event(VK_LEFT.0, true, true));
    }
    out.push(vk_event(VK_SHIFT.0, true, false));
    out
}

/// Pure record builder for `press`: modifiers down (Ctrl, Shift, Alt), the key down/up,
/// then modifiers up in reverse order.
pub(crate) fn press_inputs(vk: u16, ctrl: bool, shift: bool, alt: bool) -> Vec<INPUT> {
    let mut mods: Vec<u16> = Vec::with_capacity(3);
    if ctrl {
        mods.push(VK_CONTROL.0);
    }
    if shift {
        mods.push(VK_SHIFT.0);
    }
    if alt {
        mods.push(VK_MENU.0);
    }
    let mut out = Vec::with_capacity(mods.len() * 2 + 2);
    for m in &mods {
        out.push(vk_event(*m, false, false));
    }
    out.push(vk_event(vk, false, is_extended(vk)));
    out.push(vk_event(vk, true, is_extended(vk)));
    for m in mods.iter().rev() {
        out.push(vk_event(*m, true, false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (wVk, wScan, dwFlags, dwExtraInfo)
    fn fields(i: &INPUT) -> (u16, u16, u32, usize) {
        assert_eq!(i.r#type, INPUT_KEYBOARD);
        // SAFETY: every record built by this module sets the keyboard arm of the union.
        let k = unsafe { i.Anonymous.ki };
        (k.wVk.0, k.wScan, k.dwFlags.0, k.dwExtraInfo)
    }

    fn is_up(flags: u32) -> bool {
        flags & KEYEVENTF_KEYUP.0 != 0
    }

    fn is_unicode(flags: u32) -> bool {
        flags & KEYEVENTF_UNICODE.0 != 0
    }

    #[test]
    fn unicode_counts_and_flags() {
        let v = unicode_inputs("ab");
        assert_eq!(v.len(), 4);
        let f: Vec<_> = v.iter().map(fields).collect();
        assert_eq!(f[0].1, 'a' as u16);
        assert!(is_unicode(f[0].2) && !is_up(f[0].2));
        assert_eq!(f[1].1, 'a' as u16);
        assert!(is_unicode(f[1].2) && is_up(f[1].2));
        assert_eq!(f[2].1, 'b' as u16);
        assert!(!is_up(f[2].2));
        assert!(is_up(f[3].2));
        assert!(f.iter().all(|x| x.0 == 0 && x.3 == INJECT_TAG));
    }

    #[test]
    fn newline_is_vk_return_and_cr_is_skipped() {
        for text in ["\n", "\r\n"] {
            let v = unicode_inputs(text);
            assert_eq!(v.len(), 2, "text {:?}", text);
            let f: Vec<_> = v.iter().map(fields).collect();
            assert_eq!(f[0].0, VK_RETURN.0);
            assert!(!is_unicode(f[0].2) && !is_up(f[0].2));
            assert_eq!(f[1].0, VK_RETURN.0);
            assert!(!is_unicode(f[1].2) && is_up(f[1].2));
            assert!(f.iter().all(|x| x.3 == INJECT_TAG));
        }
        assert!(unicode_inputs("\r").is_empty());
    }

    #[test]
    fn surrogate_pair_becomes_two_units() {
        let v = unicode_inputs("😀");
        assert_eq!(v.len(), 4);
        let f: Vec<_> = v.iter().map(fields).collect();
        let units: Vec<u16> = "😀".encode_utf16().collect();
        assert_eq!(units, vec![0xD83D, 0xDE00]);
        assert_eq!(f[0].1, 0xD83D);
        assert_eq!(f[1].1, 0xD83D);
        assert_eq!(f[2].1, 0xDE00);
        assert_eq!(f[3].1, 0xDE00);
        assert!(!is_up(f[0].2) && is_up(f[1].2));
        assert!(!is_up(f[2].2) && is_up(f[3].2));
    }

    #[test]
    fn unicode_batches_start_on_key_down() {
        // 100 chars = 200 records. Every 64-record chunk must start on a key down so a
        // char is never split across batches.
        let text: String = std::iter::repeat('x').take(100).collect();
        let v = unicode_inputs(&text);
        assert_eq!(v.len(), 200);
        for chunk in v.chunks(MAX_BATCH) {
            assert!(chunk.len() <= MAX_BATCH);
            assert!(!is_up(fields(&chunk[0]).2));
        }
    }

    #[test]
    fn backspace_records() {
        let v = backspace_inputs(3);
        assert_eq!(v.len(), 6);
        for (i, rec) in v.iter().enumerate() {
            let (vk, _, flags, extra) = fields(rec);
            assert_eq!(vk, VK_BACK.0);
            assert_eq!(is_up(flags), i % 2 == 1);
            assert_eq!(flags & KEYEVENTF_EXTENDEDKEY.0, 0);
            assert_eq!(extra, INJECT_TAG);
        }
        assert!(backspace_inputs(0).is_empty());
    }

    #[test]
    fn select_back_records() {
        let v = select_back_inputs(3);
        assert_eq!(v.len(), 8);
        let f: Vec<_> = v.iter().map(fields).collect();
        assert_eq!(f[0].0, VK_SHIFT.0);
        assert!(!is_up(f[0].2));
        for i in 0..3 {
            let (vk, _, flags, _) = f[1 + i * 2];
            assert_eq!(vk, VK_LEFT.0);
            assert!(!is_up(flags));
            assert_ne!(flags & KEYEVENTF_EXTENDEDKEY.0, 0);
            let (vk, _, flags, _) = f[2 + i * 2];
            assert_eq!(vk, VK_LEFT.0);
            assert!(is_up(flags));
            assert_ne!(flags & KEYEVENTF_EXTENDEDKEY.0, 0);
        }
        assert_eq!(f[7].0, VK_SHIFT.0);
        assert!(is_up(f[7].2));
        assert!(f.iter().all(|x| x.3 == INJECT_TAG));
        assert!(select_back_inputs(0).is_empty());
    }

    #[test]
    fn press_chord_order() {
        let v = press_inputs(VK_RIGHT.0, true, true, true);
        let seq: Vec<(u16, bool)> = v
            .iter()
            .map(|r| {
                let (vk, _, flags, _) = fields(r);
                (vk, is_up(flags))
            })
            .collect();
        assert_eq!(
            seq,
            vec![
                (VK_CONTROL.0, false),
                (VK_SHIFT.0, false),
                (VK_MENU.0, false),
                (VK_RIGHT.0, false),
                (VK_RIGHT.0, true),
                (VK_MENU.0, true),
                (VK_SHIFT.0, true),
                (VK_CONTROL.0, true),
            ]
        );
        // VK_RIGHT is an extended key.
        assert_ne!(fields(&v[3]).2 & KEYEVENTF_EXTENDEDKEY.0, 0);
    }

    #[test]
    fn press_without_modifiers() {
        let v = press_inputs(0x41, false, false, false);
        assert_eq!(v.len(), 2);
        assert_eq!(fields(&v[0]).0, 0x41);
        assert!(!is_up(fields(&v[0]).2));
        assert!(is_up(fields(&v[1]).2));
    }
}
