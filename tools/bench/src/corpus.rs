//! Corpus loading: `*.md` / `*.txt` files -> filtered sentences -> an evenly spaced eval set.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// Sentences outside this inclusive word range are not eligible as eval targets.
pub const MIN_WORDS: usize = 6;
pub const MAX_WORDS: usize = 40;
/// A sentence with more than this share of non-letters (among non-whitespace chars) is dropped.
const MAX_NON_LETTER_SHARE: f64 = 0.20;
/// Words that end in `.` without ending the sentence.
const ABBREVIATIONS: [&str; 10] = ["e.g", "i.e", "vs", "cf", "mr", "mrs", "ms", "dr", "approx", "fig"];

/// One eval target: a sentence plus the (up to two) sentences before it in the same file.
#[derive(Debug, Clone)]
pub struct Example {
    pub file: String,
    /// Index of the sentence among the file's kept sentences.
    pub index: usize,
    pub sentence: String,
    /// Previous sentences, oldest first.
    pub context: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CorpusStats {
    pub files: usize,
    /// Sentences kept after the code/table/path/URL filters (any length).
    pub sentences: usize,
    /// Of those, the ones with `MIN_WORDS..=MAX_WORDS` words.
    pub eligible: usize,
    pub selected: usize,
}

/// Load the corpus under `path` (a directory, or a single file) and pick up to `max`
/// evenly spaced eval sentences.
pub fn load_examples(path: &Path, max: usize) -> Result<(Vec<Example>, CorpusStats)> {
    let mut files = Vec::new();
    if path.is_file() {
        files.push(path.to_path_buf());
    } else {
        collect_files(path, &mut files)?;
    }
    let mut stats = CorpusStats { files: files.len(), ..Default::default() };
    let mut candidates = Vec::new();
    for f in &files {
        let bytes = std::fs::read(f).with_context(|| format!("reading {}", f.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        let sentences = file_sentences(text.trim_start_matches('\u{feff}'));
        stats.sentences += sentences.len();
        let rel = match f.strip_prefix(path) {
            Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().replace('\\', "/"),
            _ => f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        for (i, s) in sentences.iter().enumerate() {
            let words = s.split_whitespace().count();
            if (MIN_WORDS..=MAX_WORDS).contains(&words) {
                candidates.push(Example {
                    file: rel.clone(),
                    index: i,
                    sentence: s.clone(),
                    context: sentences[i.saturating_sub(2)..i].to_vec(),
                });
            }
        }
    }
    stats.eligible = candidates.len();
    let selected = evenly_spaced(candidates, max);
    stats.selected = selected.len();
    Ok((selected, stats))
}

/// `n` items spread evenly across `v` (all of it when it is not longer than `n`).
pub fn evenly_spaced<T: Clone>(v: Vec<T>, n: usize) -> Vec<T> {
    if n == 0 {
        return Vec::new();
    }
    if v.len() <= n {
        return v;
    }
    (0..n).map(|k| v[k * v.len() / n].clone()).collect()
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if e.file_type()?.is_dir() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if !matches!(name.as_str(), ".git" | "node_modules" | "target") {
                collect_files(&p, out)?;
            }
        } else if p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("md") || x.eq_ignore_ascii_case("txt"))
        {
            out.push(p);
        }
    }
    Ok(())
}

/// All kept sentences of one file, in order. Lines that look like code, tables, paths or
/// URLs are dropped whole; so are sentences that are mostly non-letters.
pub fn file_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in text.lines() {
        let t = raw.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line_has_code_markers(t) {
            continue;
        }
        let line = strip_markdown_prefix(t).replace('*', "");
        for s in split_line(&line) {
            let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if !s.is_empty() && !mostly_non_letters(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// `|` (tables), backticks (inline code), `://` (URLs), `\` (Windows paths), or a word with
/// two or more `/` (Unix-style paths).
pub fn line_has_code_markers(line: &str) -> bool {
    line.contains(['|', '`', '\\'])
        || line.contains("://")
        || line.split_whitespace().any(|w| w.matches('/').count() >= 2)
}

/// More than 20% of the non-whitespace characters are not letters (digits and punctuation count).
pub fn mostly_non_letters(s: &str) -> bool {
    let total = s.chars().filter(|c| !c.is_whitespace()).count();
    if total == 0 {
        return true;
    }
    let non_letters = s.chars().filter(|c| !c.is_whitespace() && !c.is_alphabetic()).count();
    non_letters as f64 / total as f64 > MAX_NON_LETTER_SHARE
}

fn strip_markdown_prefix(t: &str) -> &str {
    let mut s = t.trim_start_matches('#').trim_start();
    s = s.trim_start_matches('>').trim_start();
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(bullet) {
            s = rest.trim_start();
            break;
        }
    }
    // Task-list checkboxes.
    for check in ["[ ]", "[x]", "[X]"] {
        if let Some(rest) = s.strip_prefix(check) {
            return rest.trim_start();
        }
    }
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &s[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    s
}

fn is_abbreviation(sentence_so_far: &str) -> bool {
    let last = sentence_so_far.split_whitespace().next_back().unwrap_or("");
    let last = last.trim_end_matches('.').to_lowercase();
    ABBREVIATIONS.contains(&last.as_str())
}

/// Split one line at `.`, `!` and `?` that are followed by whitespace or the end of the
/// line (so `3.5` and `lib.rs` stay whole). Trailing quotes/brackets stay with the sentence.
fn split_line(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        cur.push(c);
        i += 1;
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        while i < chars.len()
            && matches!(chars[i], '.' | '!' | '?' | '"' | '\'' | ')' | ']' | '\u{201d}' | '\u{2019}')
        {
            cur.push(chars[i]);
            i += 1;
        }
        let at_boundary = i >= chars.len() || chars[i].is_whitespace();
        if at_boundary && !(c == '.' && is_abbreviation(&cur)) {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_terminators_followed_by_space() {
        assert_eq!(
            split_line("It works. Really? Yes! Version 3.5 of lib.rs is out."),
            ["It works.", " Really?", " Yes!", " Version 3.5 of lib.rs is out."]
        );
    }

    #[test]
    fn abbreviations_do_not_split() {
        assert_eq!(split_line("Use a tool, e.g. a hammer. Done."), ["Use a tool, e.g. a hammer.", " Done."]);
    }

    #[test]
    fn closing_quotes_stay_with_the_sentence() {
        assert_eq!(split_line("He said \"stop.\" Then left."), ["He said \"stop.\"", " Then left."]);
    }

    #[test]
    fn drops_code_tables_paths_and_urls() {
        for bad in ["| a | b |", "use `foo` here", "see https://x.y/z now", "C:\\dir\\file", "crates/cc-engine/src/lib.rs is it"] {
            assert!(line_has_code_markers(bad), "{bad}");
        }
        assert!(!line_has_code_markers("and/or maybe not so much"));
        assert!(mostly_non_letters("1234 5678 9 + 11 == 22"));
        assert!(!mostly_non_letters("The quick brown fox, it's quick."));
    }

    #[test]
    fn markdown_prefixes_are_stripped() {
        assert_eq!(strip_markdown_prefix("## Heading text"), "Heading text");
        assert_eq!(strip_markdown_prefix("- a bullet"), "a bullet");
        assert_eq!(strip_markdown_prefix("12. numbered item"), "numbered item");
        assert_eq!(strip_markdown_prefix("> quoted"), "quoted");
        assert_eq!(strip_markdown_prefix("- [ ] open task"), "open task");
        assert_eq!(strip_markdown_prefix("[x] done task"), "done task");
    }

    #[test]
    fn file_sentences_skips_fences_and_code_lines() {
        let text = "# Title here\n\nFirst sentence is plain. Second one too!\n```\nlet x = 1;\n```\n| a | b |\n- a **bold** bullet point.\n";
        assert_eq!(
            file_sentences(text),
            ["Title here", "First sentence is plain.", "Second one too!", "a bold bullet point."]
        );
    }

    #[test]
    fn evenly_spaced_picks_spread_indices() {
        let v: Vec<usize> = (0..10).collect();
        assert_eq!(evenly_spaced(v.clone(), 5), [0, 2, 4, 6, 8]);
        assert_eq!(evenly_spaced(v.clone(), 20), v);
        assert!(evenly_spaced(v, 0).is_empty());
    }
}
