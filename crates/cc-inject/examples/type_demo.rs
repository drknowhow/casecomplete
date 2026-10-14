//! Manual demo: focus a text field (e.g. Notepad) within 3 seconds of launching.
//! Run with: `cargo run -p cc-inject --example type_demo`

use std::{thread, time::Duration};

use cc_inject::{backspace, press, select_back, type_text};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RIGHT;

fn main() -> anyhow::Result<()> {
    eprintln!("Focus the target window; typing starts in 3 seconds...");
    thread::sleep(Duration::from_secs(3));
    type_text("Hello from cc-inject ✓ émoji 😀\nsecond line")?;
    backspace(4)?;
    select_back(3)?;
    press(VK_RIGHT.0, false, false, false)?;
    Ok(())
}
