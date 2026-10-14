//! Shows the tray icon for 30 seconds, printing every event.
//! The status line gets a counter every 5 seconds. Exits early on "Quit".

use std::time::{Duration, Instant};

use cc_tray::{Tray, TrayEvent, TrayState};
use crossbeam_channel::RecvTimeoutError;

const RUN_FOR: Duration = Duration::from_secs(30);
const STATUS_EVERY: Duration = Duration::from_secs(5);

fn main() -> anyhow::Result<()> {
    let mut state = TrayState {
        enabled: true,
        auto_fix_level: "S0".to_owned(),
        status_line: "Ready - 0 fixes kept today".to_owned(),
    };
    let tray = Tray::new(state.clone())?;
    println!("tray icon created; running for {} s (choose Quit to stop early)", RUN_FOR.as_secs());

    let start = Instant::now();
    let deadline = start + RUN_FOR;
    let mut next_status = start + STATUS_EVERY;
    let mut counter = 0u32;

    loop {
        let now = Instant::now();
        if now >= deadline {
            println!("30 s elapsed, exiting");
            break;
        }
        match tray.events().recv_timeout(next_status.min(deadline) - now) {
            Ok(event) => {
                println!("[{:>5.1}s] event: {event:?}", start.elapsed().as_secs_f32());
                match event {
                    TrayEvent::Quit => break,
                    TrayEvent::Enabled(on) => state.enabled = on,
                    TrayEvent::AutoFixLevel(level) => state.auto_fix_level = level,
                    TrayEvent::PauseMinutes(_) => {
                        // Pretend the app paused itself and mirror that in the menu.
                        state.enabled = false;
                        state.status_line = String::new();
                        tray.set_state(&state);
                    }
                    TrayEvent::OpenProfileFolder | TrayEvent::ShowMetrics => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                println!("tray thread ended unexpectedly");
                break;
            }
        }
        if Instant::now() >= next_status {
            counter += 1;
            tray.set_status(&format!("Ready - {counter} fixes kept today"));
            next_status += STATUS_EVERY;
        }
    }

    drop(tray);
    println!("tray icon removed");
    Ok(())
}
