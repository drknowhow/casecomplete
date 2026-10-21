//! Measures what auto-fix does with typos in running text: it puts one keyboard slip into a
//! word of each sentence, runs the word-boundary decision the app makes (with and without
//! the language model choosing between candidates), and counts right fixes, wrong fixes
//! and words left alone.
//!
//! cargo run --release -p cc-app --example fix_eval -- http://127.0.0.1:8090 <text files...>
//!
//! The server is a llama-server with the completion model loaded. Plain prose works as
//! input; Harper's test texts (harper-core/tests/text/*.md) are what the thresholds were
//! tuned on.

#[allow(dead_code)]
#[path = "../src/fixgate.rs"]
mod fixgate;

use cc_correct::{AutocorrectList, Checker, PersonalDictionary, Severity};
use cc_engine::CompletionClient;
use std::collections::BTreeMap;
use std::path::Path;

const LEVEL: Severity = Severity::S2;
const CONTEXT_CHARS: usize = 300;
const NEXT_TOKENS: u32 = 40;
const SEED: u64 = 0x5eed_cafe;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const ROWS: [(&str, f32); 3] = [("qwertyuiop", 0.0), ("asdfghjkl", 0.25), ("zxcvbnm", 0.75)];

/// Keys next to `c` on a QWERTY keyboard, as cc-correct's typing model counts them.
fn neighbours(c: char) -> Vec<char> {
    let pos = |c: char| {
        ROWS.iter().enumerate().find_map(|(y, (row, offset))| row.find(c).map(|x| (x as f32 + offset, y as f32)))
    };
    let Some((x, y)) = pos(c) else { return Vec::new() };
    ROWS.iter()
        .flat_map(|(row, _)| row.chars())
        .filter(|&k| k != c && pos(k).is_some_and(|(kx, ky)| (kx - x).powi(2) + (ky - y).powi(2) <= 2.0))
        .collect()
}

/// One slip of the given kind in `word`, or None when it does not apply.
fn slip(word: &str, kind: &str, rng: &mut Rng) -> Option<String> {
    let mut w: Vec<char> = word.chars().collect();
    let n = w.len();
    match kind {
        "neighbour" => {
            let i = rng.below(n);
            let near = neighbours(w[i]);
            if near.is_empty() {
                return None;
            }
            w[i] = near[rng.below(near.len())];
        }
        "swap" => {
            let i = rng.below(n - 1);
            if w[i] == w[i + 1] {
                return None;
            }
            w.swap(i, i + 1);
        }
        "drop" => {
            w.remove(rng.below(n));
        }
        "extra" => {
            let i = rng.below(n);
            let near = neighbours(w[i]);
            if near.is_empty() {
                return None;
            }
            w.insert(i + rng.below(2), near[rng.below(near.len())]);
        }
        "double" => {
            let i = rng.below(n);
            w.insert(i, w[i]);
        }
        _ => unreachable!(),
    }
    Some(w.into_iter().collect())
}

/// Sentences of plain prose, each with the text before it in its paragraph.
fn sentences(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for para in text.split("\n\n") {
        let para: String = para
            .lines()
            .filter(|l| !l.trim_start().starts_with(['#', '|', '>', '-', '*', '`']))
            .collect::<Vec<_>>()
            .join(" ");
        let para = para.replace(['_', '*', '[', ']', '(', ')'], "");
        let mut before = String::new();
        for s in para.split_inclusive(['.', '!', '?']) {
            let s = s.trim();
            let words = s.split_whitespace().count();
            if (6..=40).contains(&words) && s.chars().all(|c| c.is_ascii()) {
                out.push((before.clone(), s.to_string()));
            }
            before.push_str(s);
            before.push(' ');
        }
    }
    out
}

#[derive(Default)]
struct Tally {
    right: usize,
    wrong: usize,
    left: usize,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (server, files) = args.split_first().expect("usage: fix_eval <server url> <text files...>");
    let checker = Checker::new();
    let dict = PersonalDictionary::default();
    let auto = AutocorrectList::load(Path::new("nonexistent-autocorrect.txt"));
    let client = CompletionClient::new(server);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let mut rng = Rng(SEED);
    let kinds = ["neighbour", "swap", "drop", "extra", "double"];

    let mut tables: BTreeMap<String, BTreeMap<String, Tally>> = BTreeMap::new();
    let mut wrong_lines = Vec::new();
    let mut samples = 0;
    for file in files {
        let text = std::fs::read_to_string(file)?;
        for (para_before, sentence) in sentences(&text) {
            let words: Vec<&str> = sentence.split(' ').collect();
            // One target per sentence: a lowercase dictionary word of four or more letters,
            // not the first word.
            let targets: Vec<usize> = (1..words.len())
                .filter(|&i| {
                    let w = words[i];
                    w.len() >= 4 && w.chars().all(|c| c.is_ascii_lowercase()) && checker.is_word(w, &dict)
                })
                .collect();
            if targets.is_empty() {
                continue;
            }
            let i = targets[rng.below(targets.len())];
            let truth = words[i];
            let kind = kinds[rng.below(kinds.len())];
            let Some(typo) = slip(truth, kind, &mut rng) else { continue };
            if typo.len() < 3 || checker.is_word(&typo, &dict) {
                continue;
            }
            samples += 1;
            let mut body = format!("{para_before}{}", words[..i].join(" "));
            if i > 0 {
                body.push(' ');
            }
            body.push_str(&typo);
            let tail_start = body.len().saturating_sub(CONTEXT_CHARS);
            let tail_start = (tail_start..body.len()).find(|&k| body.is_char_boundary(k)).unwrap_or(0);
            let body = &body[tail_start..];
            let start = body.rfind(['.', '!', '?', '\n']).map_or(0, |k| k + 1);
            let sent = body[start..].trim_start();
            let n_chars = sent.chars().count();
            let before = body[..body.len() - typo.len()].trim_end_matches(' ');

            let listed = auto.lookup(&typo);
            let found = checker.check(sent, &dict);
            let at_end = |c: &&cc_correct::Candidate| c.end == n_chars && sent.ends_with(&c.original);
            let eligible = found
                .iter()
                .filter(at_end)
                .find(|c| fixgate::eligible(&c.severity, &c.rule, &c.original, &c.replacement, &LEVEL))
                .filter(|c| fixgate::reject(&c.original, &c.replacement, true, false, |t| checker.is_word(t, &dict)).is_none());
            let guess = found.iter().filter(at_end).find(|c| c.rule == "spell" && c.severity == Severity::S2);
            let needs_model = listed.is_none() && (eligible.is_some_and(|c| c.rule == "spell") || (eligible.is_none() && guess.is_some()));
            let top = if needs_model && !before.trim().is_empty() {
                rt.block_on(client.next_tokens(before, NEXT_TOKENS)).unwrap_or_default()
            } else {
                Vec::new()
            };
            let is_word = |w: &str| checker.is_word(w, &dict);
            let options = checker.typo_options(&typo, &dict);

            // 0.1.3: the rules, with the model's veto on guessed S0 fixes.
            let base_fix: Option<String> = if let Some(fix) = listed.clone() {
                Some(fix)
            } else if let Some(c) = eligible {
                let guessed = c.rule == "spell" && c.severity == Severity::S0 && !cc_correct::is_doubled_letter_typo(&c.original, &c.replacement);
                if guessed && (top.is_empty() || fixgate::model_prefers(&c.original, &c.replacement, &top, is_word).is_some()) {
                    None
                } else {
                    Some(c.replacement.clone())
                }
            } else {
                None
            };
            // New: the model picks between the typo's options where it clearly expects one.
            let decide = |picked: Option<String>, doubled_too: bool| -> Option<String> {
                if let Some(fix) = listed.clone() {
                    Some(fix)
                } else if let Some(c) = eligible {
                    let guessed = c.rule == "spell" && c.severity == Severity::S0 && !cc_correct::is_doubled_letter_typo(&c.original, &c.replacement);
                    if !guessed {
                        // A doubled-key fix is never held back, but the model may pick another spelling.
                        match picked {
                            Some(p) if doubled_too && c.rule == "spell" => Some(p),
                            _ => Some(c.replacement.clone()),
                        }
                    } else if top.is_empty() {
                        None
                    } else if picked.is_some() {
                        picked
                    } else if fixgate::model_prefers(&c.original, &c.replacement, &top, is_word).is_some() {
                        None
                    } else {
                        Some(c.replacement.clone())
                    }
                } else if guess.is_some() {
                    picked
                } else {
                    None
                }
            };
            let pick_fix = decide(fixgate::model_pick(&typo, &options, &top), false);
            let mut fixes = vec![
                ("0.1.3 (rules + veto)".to_string(), base_fix.clone()),
                ("model picks".to_string(), pick_fix.clone()),
                ("model picks, doubled keys too".to_string(), decide(fixgate::model_pick(&typo, &options, &top), true)),
            ];
            for weight in [1.0, 2.0, 3.0] {
                for typed in [true, false] {
                    let name = format!("weight {weight}, typed word {}", if typed { "rivals" } else { "ignored" });
                    fixes.push((name, decide(fixgate::model_pick_tuned(&typo, &options, &top, weight, typed), false)));
                }
            }
            for (name, fix) in &fixes {
                let table = tables.entry(name.clone()).or_default();
                for key in [kind.to_string(), "all".to_string()] {
                    let t = table.entry(key).or_default();
                    match fix {
                        None => t.left += 1,
                        Some(f) if f.eq_ignore_ascii_case(truth) => t.right += 1,
                        Some(_) => t.wrong += 1,
                    }
                }
            }
            if let Some(f) = &pick_fix {
                if !f.eq_ignore_ascii_case(truth) {
                    let ctx: String = before.chars().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
                    wrong_lines.push(format!(
                        "  {kind:9} …{ctx} [{typo}] -> {f:?} (meant {truth:?}; 0.1.3: {base_fix:?})"
                    ));
                }
            }
        }
    }
    println!("{samples} typos");
    for (name, table) in &tables {
        let all = &table["all"];
        println!("{name:36} right {:4}  wrong {:4}  left {:4}", all.right, all.wrong, all.left);
    }
    for name in ["0.1.3 (rules + veto)", "model picks"] {
        println!("{name}:");
        for (kind, t) in &tables[name] {
            println!("  {kind:9} right {:4}  wrong {:4}  left {:4}", t.right, t.wrong, t.left);
        }
    }
    println!("wrong fixes with model picks:");
    for line in wrong_lines {
        println!("{line}");
    }
    Ok(())
}
