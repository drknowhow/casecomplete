//! Print what the profile's `events.db` has learned so far.
//!
//! ```text
//! cargo run -p cc-store --example store_dump [path\to\events.db]
//! ```
//!
//! Defaults to `%LOCALAPPDATA%\CaseComplete\profiles\default\events.db`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use cc_store::{Metrics, Store};
use rusqlite::{Connection, OpenFlags};

fn default_path() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(base)
        .join("CaseComplete")
        .join("profiles")
        .join("default")
        .join("events.db"))
}

fn print_metrics(label: &str, m: &Metrics) {
    println!("{label}");
    println!("  events                 {}", m.events);
    println!("  chars saved            {}", m.chars_saved);
    println!("  auto-fixes             {} ({} reverted)", m.fixes_auto, m.fixes_reverted);
    println!("  suggestions accepted   {}", m.suggestions_accepted);
    println!(
        "  completions            {} shown, {} accepted",
        m.completions_shown, m.completions_accepted
    );
    println!("  latency                p50 {} ms, p95 {} ms", m.p50_latency_ms, m.p95_latency_ms);
}

fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('~');
    }
    format!("{out:?}")
}

fn main() -> Result<()> {
    let path = match std::env::args_os().nth(1) {
        Some(p) => PathBuf::from(p),
        None => default_path()?,
    };
    if !path.exists() {
        println!("no events.db at {}", path.display());
        return Ok(());
    }
    println!("{}\n", path.display());

    let store = Store::open(&path)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    print_metrics("metrics (all time)", &store.metrics(None)?);
    println!();
    print_metrics("metrics (last 24 h)", &store.metrics(Some(now - 24 * 3_600_000))?);

    println!("\nrules");
    let stats = store.rule_stats(1)?;
    if stats.is_empty() {
        println!("  (none)");
    } else {
        println!(
            "  {:<28} {:>6} {:>9} {:>10} {:>9}  state",
            "rule", "auto", "reverted", "suggested", "accepted"
        );
        for r in &stats {
            let state = if store.is_demoted(&r.rule)? { "demoted" } else { "" };
            println!(
                "  {:<28} {:>6} {:>9} {:>10} {:>9}  {}",
                r.rule, r.auto_total, r.auto_reverted, r.suggested_total, r.suggested_accepted, state
            );
        }
    }

    println!("\nlast 20 events (newest first)");
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT id, datetime(ts / 1000, 'unixepoch', 'localtime'), app_exe, source, rule, \
                severity, confidence, action, COALESCE(outcome, '-'), latency_ms, \
                original, replacement \
         FROM events ORDER BY id DESC LIMIT 20",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(format!(
            "  #{:<5} {} {:<14} {:<12} {:<22} {} {:.2} {:<9} {:<11} {:>4}ms {} -> {}",
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, f64>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, String>(8)?,
            r.get::<_, i64>(9)?,
            clip(&r.get::<_, String>(10)?, 24),
            clip(&r.get::<_, String>(11)?, 48),
        ))
    })?;
    let mut any = false;
    for line in rows {
        println!("{}", line?);
        any = true;
    }
    if !any {
        println!("  (none)");
    }
    Ok(())
}
