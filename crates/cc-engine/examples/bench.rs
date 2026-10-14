//! Latency benchmark for the completion sidecar.
//!
//! `cargo run -p cc-engine --example bench --release [-- <root>]`
//!
//! Provisions llama-server + model under `<root>` (default `%LOCALAPPDATA%\CaseComplete`),
//! starts the sidecar on port 8089, runs one unmeasured warm-up request, then times five
//! short English prefixes (n_predict = 24) and prints a table.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cc_engine::{ensure_assets, CompletionClient, Sidecar};
use tokio_util::sync::CancellationToken;

const PORT: u16 = 8089;
const N_PREDICT: u32 = 24;

const PROMPTS: [&str; 5] = [
    "The quick brown fox jumps over the",
    "I wanted to let you know that the meeting has been",
    "Thank you so much for your help with",
    "After the rain stopped, we walked down to the",
    "She opened the old wooden box and found",
];

fn default_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("CaseComplete")
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn clip(s: &str, n: usize) -> String {
    let flat = s.replace('\n', " ");
    if flat.chars().count() <= n {
        flat
    } else {
        format!("{}...", flat.chars().take(n.saturating_sub(3)).collect::<String>())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let root = std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(default_root);
    println!("root: {}", root.display());

    let mut last_label = String::new();
    let t_assets = Instant::now();
    let assets = ensure_assets(&root, |label, done, total| {
        if label != last_label {
            if !last_label.is_empty() {
                println!();
            }
            last_label = label.to_string();
        }
        match total {
            Some(t) if t > 0 => print!(
                "\r  {label}: {:8.1} / {:8.1} MB ({:5.1}%)   ",
                mb(done),
                mb(t),
                done as f64 * 100.0 / t as f64
            ),
            _ => print!("\r  {label}: {:8.1} MB   ", mb(done)),
        }
        let _ = std::io::stdout().flush();
    })
    .await?;
    println!();
    println!("assets ready in {:.1}s", t_assets.elapsed().as_secs_f64());
    println!("  server: {}", assets.server_exe.display());
    println!("  model:  {}", assets.model.display());
    if let Ok(version) = std::fs::read_to_string(root.join("bin").join("VERSION")) {
        println!("  llama.cpp release: {}", version.trim());
    }

    let t_start = Instant::now();
    let sidecar = Sidecar::start(&assets, PORT).await?;
    println!("sidecar healthy at {} after {:.2}s", sidecar.base_url(), t_start.elapsed().as_secs_f64());

    let client = CompletionClient::new(&sidecar.base_url());

    // Warm-up: first request pays for kernel/cache initialisation; not part of the table.
    let t = Instant::now();
    let warm = client
        .complete("Once upon a", N_PREDICT, CancellationToken::new(), |_| {})
        .await?;
    println!("warm-up request: {:.0} ms ({:?})", ms(t.elapsed()), clip(&warm.text, 40));
    println!();

    println!(
        "{:>2}  {:<34} {:>9} {:>9} {:>7} {:>9} {:>9} {:>8} {:>4}  completion",
        "#", "prompt", "ttft ms", "total ms", "tokens", "srv pp ms", "srv tg ms", "mean lp", "eos"
    );
    let mut rows = Vec::new();
    for (i, prompt) in PROMPTS.iter().enumerate() {
        let start = Instant::now();
        let mut first: Option<Duration> = None;
        let mut tokens = 0u32;
        let c = client
            .complete(prompt, N_PREDICT, CancellationToken::new(), |_| {
                first.get_or_insert_with(|| start.elapsed());
                tokens += 1;
            })
            .await?;
        let total = start.elapsed();
        let ttft = first.unwrap_or(total);
        println!(
            "{:>2}  {:<34} {:>9.1} {:>9.1} {:>7} {:>9.1} {:>9.1} {:>8} {:>4}  {}",
            i + 1,
            clip(prompt, 34),
            ms(ttft),
            ms(total),
            tokens,
            c.prompt_ms,
            c.predicted_ms,
            c.mean_logprob.map_or("-".to_string(), |lp| format!("{lp:.3}")),
            if c.stopped_at_sentence_end { "yes" } else { "no" },
            clip(&c.text, 60),
        );
        rows.push((ms(ttft), ms(total)));
    }

    let n = rows.len() as f64;
    println!(
        "\nmean ttft {:.1} ms, mean total {:.1} ms over {} prompts (n_predict={})",
        rows.iter().map(|r| r.0).sum::<f64>() / n,
        rows.iter().map(|r| r.1).sum::<f64>() / n,
        rows.len(),
        N_PREDICT
    );

    drop(sidecar);
    Ok(())
}
