//! Manual check for cc-overlay: ghost text follows the mouse for 5 s (repositioned every
//! 500 ms, with a small offset pattern so it visibly moves even if the mouse is still),
//! then an underline fades out under the last position, then the overlay shuts down.
//!
//! Run: `cargo run -p cc-overlay --example demo`

use std::thread::sleep;
use std::time::{Duration, Instant};

use cc_overlay::{GhostStyle, Overlay, Rect};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

const TEXT: &str = "this is ghost text";

fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

fn main() -> anyhow::Result<()> {
    let overlay = Overlay::new()?;
    let style = GhostStyle::default();
    let offsets = [(16, 20), (64, 20), (64, 52), (16, 52)];

    let started = Instant::now();
    let mut last = (0, 0);
    for step in 0..10 {
        let (cx, cy) = cursor();
        let (dx, dy) = offsets[step % offsets.len()];
        last = (cx + dx, cy + dy);
        overlay.show_ghost(TEXT, last.0, last.1, &style);
        println!("step {step}: ghost at {last:?}");
        sleep(Duration::from_millis(500));
    }

    // Pretend the ghost was accepted: hide it and mark the inserted text with a fading underline.
    overlay.hide();
    let approx_w = (TEXT.chars().count() as f32 * style.font_px * 0.5).round() as i32;
    let line_h = (style.font_px * 1.35).round() as i32;
    let fade_ms = 1500;
    overlay.show_underline(Rect { x: last.0, y: last.1, w: approx_w, h: line_h }, fade_ms);
    println!("underline fading over {fade_ms} ms");
    sleep(Duration::from_millis(u64::from(fade_ms) + 300));

    drop(overlay);
    println!("demo finished cleanly after {:.1} s", started.elapsed().as_secs_f32());
    Ok(())
}
