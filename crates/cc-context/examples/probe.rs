//! Polls cc-context once a second for 15 s so you can click around apps and see what
//! each one exposes.
//!
//!   cargo run -p cc-context --example probe
//!   RUST_LOG=cc_context=debug cargo run -p cc-context --example probe   (failure reasons)

use std::time::{Duration, Instant};

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    cc_context::init_thread()?;

    for tick in 0..15 {
        let started = Instant::now();

        let t = Instant::now();
        let focus = cc_context::focus_info();
        let t_focus = t.elapsed();

        let t = Instant::now();
        let caret = cc_context::caret_rect();
        let t_caret = t.elapsed();

        let t = Instant::now();
        let font = cc_context::font_at_caret();
        let t_font = t.elapsed();

        let t = Instant::now();
        let text = cc_context::text_before_caret(800);
        let t_text = t.elapsed();

        println!("[{tick:>2}] focus {focus:?} ({})", ms(t_focus));
        println!("     caret {caret:?} ({})", ms(t_caret));
        println!("     font  {font:?} ({})", ms(t_font));
        match text {
            Some(s) => {
                let n = s.chars().count();
                let tail: String = s.chars().skip(n.saturating_sub(80)).collect();
                println!("     text  {tail:?} ({n} chars, {})", ms(t_text));
            }
            None => println!("     text  None ({})", ms(t_text)),
        }

        if tick < 14 {
            std::thread::sleep(Duration::from_secs(1).saturating_sub(started.elapsed()));
        }
    }
    Ok(())
}
