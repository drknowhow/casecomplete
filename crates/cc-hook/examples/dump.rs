//! Manual test: installs the keyboard hook and prints every event for 20 seconds.
//!
//! ```text
//! cargo run -p cc-hook --example dump              # observe only, nothing is swallowed
//! cargo run -p cc-hook --example dump -- --ghost   # ghost_visible=true: Tab/Ctrl+Tab/Esc are swallowed
//! cargo run -p cc-hook --example dump -- --revert  # revert window open: Left/Backspace swallowed -> RevertRequested
//! ```
//!
//! Ctrl+Alt+Z is always swallowed and reported as `RevertRequested`. Key-ups and injected
//! input are never reported. Focus any other window and type; events appear here, each
//! with the number of keys and clicks that had reached the app.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cc_hook::{now_ms, start, Stamped, SwallowState};
use crossbeam_channel::RecvTimeoutError;

const RUN_FOR: Duration = Duration::from_secs(20);

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ghost = args.iter().any(|a| a == "--ghost");
    let revert = args.iter().any(|a| a == "--revert");

    let (tx, rx) = crossbeam_channel::bounded::<Stamped>(1024);
    let swallow = Arc::new(SwallowState::default());
    swallow.set_ghost_visible(ghost);
    if revert {
        swallow.set_revert_until(now_ms() + RUN_FOR.as_millis() as u64);
    }

    let handle = start(tx, swallow.clone())?;
    println!(
        "cc-hook dump: running for {}s (ghost_visible={ghost}, revert_window={revert}). Type in another window.",
        RUN_FOR.as_secs()
    );

    let began = Instant::now();
    let deadline = began + RUN_FOR;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok((seq, ev)) => println!("[{:>6} ms] #{seq} {:?}", now_ms(), ev),
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(handle); // unhooks and joins the hook thread
    println!("cc-hook dump: done, hook removed.");
    Ok(())
}
