//! Types argv[1] into the focused window after a 2 s delay. Used by the e2e scripts.
fn main() -> anyhow::Result<()> {
    let text = std::env::args().nth(1).unwrap_or_else(|| "hello from cc-inject".into());
    std::thread::sleep(std::time::Duration::from_secs(2));
    cc_inject::type_text(&text)
}
