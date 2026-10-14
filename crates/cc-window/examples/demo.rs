//! Opens the dashboard on https://example.com, toggles the flyout on the same
//! url after 2 s and exits after 10 s. Do not click anything; just look.

use std::thread::sleep;
use std::time::{Duration, Instant};

use cc_window::Windows;

const URL: &str = "https://example.com";
const FLYOUT_AT: Duration = Duration::from_secs(2);
const RUN_FOR: Duration = Duration::from_secs(10);

fn main() -> anyhow::Result<()> {
    let start = Instant::now();
    let windows = Windows::new()?;
    println!("[{:>5.1}s] ui thread running", start.elapsed().as_secs_f32());

    windows.open_dashboard(URL);
    println!("[{:>5.1}s] dashboard requested", start.elapsed().as_secs_f32());

    sleep(FLYOUT_AT.saturating_sub(start.elapsed()));
    windows.toggle_flyout(URL);
    println!("[{:>5.1}s] flyout toggled", start.elapsed().as_secs_f32());

    sleep(RUN_FOR.saturating_sub(start.elapsed()));
    println!("[{:>5.1}s] exiting", start.elapsed().as_secs_f32());

    let dropping = Instant::now();
    drop(windows);
    println!("windows destroyed in {} ms", dropping.elapsed().as_millis());
    Ok(())
}
