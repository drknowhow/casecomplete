//! Runs the UI against a profile full of synthetic history, playing the orchestrator's part
//! (applying commands, rewriting the word files). Handy for looking at the pages:
//!
//! ```text
//! cargo run -p cc-ui --example demo -- [profile-dir]
//! ```
//!
//! Prints the flyout and dashboard addresses and serves until Ctrl-C or Quit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cc_store::Store;
use cc_ui::{ContextSnapshot, ModelInfo, UiCommand, UiServer, UiSettings, UiState};
use crossbeam_channel::unbounded;

const DAY_MS: i64 = 86_400_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn seed(db: &Path) -> anyhow::Result<()> {
    drop(Store::open(db)?);
    let conn = rusqlite::Connection::open(db)?;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let now = now_ms();
    let apps = ["WindowsTerminal.exe", "chrome.exe", "slack.exe", "OUTLOOK.EXE", "notepad.exe"];
    let fixes = [("teh", "the"), ("adn", "and"), ("recieve", "receive"), ("wierd", "weird"), ("thhe", "the"), ("dimitr", "Dimitri"), ("taht", "that"), ("seperate", "separate")];
    let rules = ["spell", "autocorrect", "sentence_capitalization", "repeated_word"];
    let texts = ["app running after a reboot.", "that would be great.", "let me know if that works.", "for the quick turnaround.", "I have been stuck on it."];
    let mut insert = conn.prepare(
        "INSERT INTO events(ts, app_exe, source, rule, severity, confidence, context_before, original, replacement, action, outcome, outcome_ts, latency_ms) \
         VALUES (?1, ?2, ?3, ?4, 'S1', ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for day in 0..30i64 {
        let span = if day == 0 { 8 * 3_600_000 } else { DAY_MS };
        // Acceptance climbs from about 18% to 34% over the month.
        let base = 0.18 + 0.16 * (1.0 - day as f64 / 29.0);
        for _ in 0..(14 + rng.below(14)) {
            let ts = now - day * DAY_MS - rng.below(span as u64) as i64;
            let app = apps[rng.below(apps.len() as u64) as usize];
            let conf = (rng.unit() * rng.unit().sqrt()).clamp(0.02, 0.98);
            let p = (base * (0.4 + conf * 1.3)).clamp(0.02, 0.9);
            let outcome = if rng.unit() < p { "accepted" } else if rng.unit() < 0.7 { "ignored" } else { "timeout" };
            let latency = 70 + rng.below(100) as i64 + if rng.below(10) == 0 { rng.below(250) as i64 } else { 0 };
            let text = texts[rng.below(texts.len() as u64) as usize];
            insert.execute(rusqlite::params![ts, app, "llm_complete", "complete", conf, "", text, "suggested", outcome, ts + 2000, latency])?;
        }
        for _ in 0..(8 + rng.below(12)) {
            let ts = now - day * DAY_MS - rng.below(span as u64) as i64;
            let app = apps[rng.below(apps.len() as u64) as usize];
            let rule = rules[rng.below(rules.len() as u64) as usize];
            let (from, to) = fixes[rng.below(fixes.len() as u64) as usize];
            let bad = if rule == "sentence_capitalization" { 0.4 } else if app == "notepad.exe" { 0.3 } else { 0.06 };
            let outcome = if rng.unit() < bad { "reverted" } else { "kept" };
            insert.execute(rusqlite::params![ts, app, "harper", rule, 0.9f64, from, to, "auto", outcome, ts + 3000, 3 + rng.below(8) as i64])?;
        }
    }
    drop(insert);
    drop(conn);
    Store::open(db)?.tune()?;
    Ok(())
}

fn read_pairs(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once("->"))
        .map(|(a, b)| (a.trim().to_lowercase(), b.trim().to_string()))
        .collect()
}

fn write_pairs(path: &Path, pairs: &BTreeMap<String, String>) {
    let body: String = pairs.iter().map(|(k, v)| format!("{k} -> {v}\n")).collect();
    let _ = std::fs::write(path, format!("# typo -> fix\n{body}"));
}

fn read_words(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(|l| l.trim().to_lowercase()).filter(|l| !l.is_empty()).collect()
}

/// What the orchestrator would do with each command.
fn apply(cmd: UiCommand, profile: &Path) {
    println!("command: {cmd:?}");
    let (dict, auto) = (profile.join("dictionary.txt"), profile.join("autocorrect.txt"));
    match cmd {
        UiCommand::AddDictionaryWord(w) => {
            let mut words = read_words(&dict);
            words.push(w.to_lowercase());
            words.sort();
            words.dedup();
            let _ = std::fs::write(&dict, words.join("\n") + "\n");
        }
        UiCommand::RemoveDictionaryWord(w) => {
            let words: Vec<String> = read_words(&dict).into_iter().filter(|x| *x != w.to_lowercase()).collect();
            let _ = std::fs::write(&dict, words.join("\n") + "\n");
        }
        UiCommand::SetAutocorrect(typo, fix) => {
            let mut pairs = read_pairs(&auto);
            pairs.insert(typo.to_lowercase(), fix);
            write_pairs(&auto, &pairs);
        }
        UiCommand::RemoveAutocorrect(typo) => {
            let mut pairs = read_pairs(&auto);
            pairs.remove(&typo.to_lowercase());
            write_pairs(&auto, &pairs);
        }
        UiCommand::Quit => std::process::exit(0),
        _ => {}
    }
}

fn main() -> anyhow::Result<()> {
    let profile: PathBuf = std::env::args_os().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("cc-ui-demo"));
    std::fs::create_dir_all(&profile)?;
    let db = profile.join("events.db");
    if !db.exists() {
        seed(&db)?;
    }
    if !profile.join("dictionary.txt").exists() {
        std::fs::write(profile.join("dictionary.txt"), "dimitri\nyepgent\ncasecomplete\n")?;
        write_pairs(&profile.join("autocorrect.txt"), &BTreeMap::from([("dimitr".to_string(), "Dimitri".to_string()), ("taht".to_string(), "that".to_string())]));
        std::fs::write(
            profile.join("privacy.toml"),
            "deny_exe = [\"1password.exe\", \"keepassxc.exe\", \"bitwarden.exe\", \"casecomplete.exe\"]\ndeny_title = [\"bank\", \"checkout\", \"password\", \"sign in\"]\nstore_context = false\n",
        )?;
        let now = now_ms();
        let log: String = [("card number", 3), ("long number", 29), ("secret after its label", 6), ("app 1password.exe", 4)]
            .iter()
            .flat_map(|(r, n)| (0..*n).map(move |i| format!("{{\"ts\":{},\"reason\":\"{r}\"}}\n", now - i * 3_600_000)))
            .collect();
        std::fs::write(profile.join("privacy_log.jsonl"), log)?;
    }

    let state = Arc::new(UiState::default());
    *state.settings.lock().unwrap() = UiSettings {
        enabled: true,
        paused_until_ms: None,
        auto_fix_level: "S1".into(),
        model: "gemma-3-1b-pt".into(),
        min_confidence: 0.12,
        models: vec![
            ModelInfo { id: "gemma-3-1b-pt".into(), label: "Gemma 3 1B".into(), origin: "Google".into(), installed: true },
            ModelInfo { id: "qwen2.5-0.5b".into(), label: "Qwen 2.5 0.5B".into(), origin: "Alibaba".into(), installed: true },
            ModelInfo { id: "llama-3.2-1b".into(), label: "Llama 3.2 1B".into(), origin: "Meta".into(), installed: false },
        ],
        profile_dir: profile.display().to_string(),
        autostart: true,
        status_line: "Ready \u{b7} Gemma 3 1B \u{b7} 118 ms".into(),
    };
    *state.last_context.lock().unwrap() = Some(ContextSnapshot {
        ts_ms: now_ms() - 90_000,
        app_exe: "WindowsTerminal.exe".into(),
        style_lines: vec!["I have been stuck on it for a while now.".into(), "Thanks for the quick turnaround on this.".into()],
        prompt_tail: "\u{2026}want us to also add a setting that keeps the".into(),
        heal: String::new(),
        need_space: true,
        completion: "app running after a reboot.".into(),
        confidence: 0.34,
        tokens: 7,
        latency_ms: 131,
        prompt_ms: 6.0,
        outcome: Some("ignored".into()),
        privacy_block: None,
    });

    let (tx, rx) = unbounded();
    let server = UiServer::start(&db, &profile, state.clone(), tx)?;
    println!("profile:   {}", profile.display());
    println!("dashboard: {}", server.url("dashboard"));
    println!("flyout:    {}", server.url("flyout"));
    for cmd in rx {
        apply(cmd, &profile);
    }
    Ok(())
}
