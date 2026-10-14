//! Read-only views of the profile files the orchestrator owns: `dictionary.txt`,
//! `autocorrect.txt`, `privacy.toml` and the optional `privacy_log.jsonl`. The formats
//! mirror what `cc-correct` and `cc-app` read and write.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// No profile file is read past this size (the privacy log keeps its newest bytes).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Entries returned per list; keeps a runaway file from producing a giant response.
pub(crate) const MAX_ENTRIES: usize = 5000;

fn read_text(path: &Path, keep_tail: bool) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let skipped = keep_tail && len > MAX_FILE_BYTES;
    if skipped {
        file.seek(SeekFrom::Start(len - MAX_FILE_BYTES)).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).ok()?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if skipped {
        // The cut usually lands mid-line; drop the partial first line.
        text = text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default();
    }
    Some(text)
}

/// Dictionary words (one per line, as `PersonalDictionary` reads them), sorted.
pub(crate) fn dictionary_words(profile: &Path) -> Vec<String> {
    let Some(text) = read_text(&profile.join("dictionary.txt"), false) else {
        return Vec::new();
    };
    let mut words: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.sort_unstable();
    words.dedup();
    words.truncate(MAX_ENTRIES);
    words
}

/// The user's `typo -> fix` pairs from `autocorrect.txt` (the built-in seed is not
/// listed), sorted by typo. Parsing matches `cc_correct::AutocorrectList`.
pub(crate) fn autocorrect_pairs(profile: &Path) -> Vec<(String, String)> {
    let Some(text) = read_text(&profile.join("autocorrect.txt"), false) else {
        return Vec::new();
    };
    let mut pairs = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((typo, fix)) = line.split_once("->") else { continue };
        let (typo, fix) = (typo.trim().to_lowercase(), fix.trim().to_string());
        if typo.is_empty() || fix.is_empty() || typo == fix.to_lowercase() {
            continue;
        }
        pairs.insert(typo, fix);
    }
    pairs.into_iter().take(MAX_ENTRIES).collect()
}

/// `privacy.toml`: where the assistant stays off.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PrivacyFile {
    /// The file exists; when false the lists are empty rather than the defaults.
    pub present: bool,
    pub deny_exe: Vec<String>,
    pub deny_title: Vec<String>,
    pub store_context: bool,
}

pub(crate) fn privacy_file(profile: &Path) -> PrivacyFile {
    match read_text(&profile.join("privacy.toml"), false) {
        Some(text) => parse_privacy(&text),
        None => PrivacyFile::default(),
    }
}

fn parse_privacy(text: &str) -> PrivacyFile {
    let mut out = PrivacyFile { present: true, ..PrivacyFile::default() };
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "deny_exe" => out.deny_exe = parse_list(value),
            "deny_title" => out.deny_title = parse_list(value),
            "store_context" => out.store_context = value == "true",
            _ => {}
        }
    }
    out
}

fn parse_list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Privacy-gate blocks counted from `privacy_log.jsonl`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PrivacyLog {
    pub present: bool,
    /// Block reason as logged (e.g. "card number") to how often it happened.
    pub by_reason: BTreeMap<String, u64>,
}

impl PrivacyLog {
    /// Blocks of text that looked sensitive (everything except app/title deny hits).
    pub fn sentences_blocked(&self) -> u64 {
        self.by_reason.iter().filter(|(r, _)| !is_app_reason(r)).map(|(_, n)| n).sum()
    }

    /// Times an app or window title on the denylist switched the assistant off.
    pub fn app_blocks(&self) -> u64 {
        self.by_reason.iter().filter(|(r, _)| is_app_reason(r)).map(|(_, n)| n).sum()
    }
}

fn is_app_reason(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    r.starts_with("app ") || r.starts_with("title")
}

/// Counts blocks logged at or after `since_ms`. Lines are JSON objects with a string
/// `reason` and optionally a `ts` in unix milliseconds (lines without one always count);
/// anything unparsable is skipped.
pub(crate) fn privacy_log(profile: &Path, since_ms: i64) -> PrivacyLog {
    let Some(text) = read_text(&profile.join("privacy_log.jsonl"), true) else {
        return PrivacyLog::default();
    };
    count_privacy_log(&text, since_ms)
}

fn count_privacy_log(text: &str, since_ms: i64) -> PrivacyLog {
    let mut log = PrivacyLog { present: true, ..PrivacyLog::default() };
    for line in text.lines() {
        let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if obj.get("ts").and_then(serde_json::Value::as_i64).is_some_and(|ts| ts < since_ms) {
            continue;
        }
        let reason = obj.get("reason").and_then(serde_json::Value::as_str).unwrap_or("").trim();
        let reason: String = if reason.is_empty() { "unknown".into() } else { reason.chars().take(80).collect() };
        *log.by_reason.entry(reason).or_insert(0) += 1;
    }
    log
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "cc-ui-profile-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_files_give_empty_views() {
        let dir = TempDir::new();
        assert!(dictionary_words(&dir.0).is_empty());
        assert!(autocorrect_pairs(&dir.0).is_empty());
        assert_eq!(privacy_file(&dir.0), PrivacyFile::default());
        assert_eq!(privacy_log(&dir.0, 0), PrivacyLog::default());
    }

    #[test]
    fn dictionary_is_lowercased_sorted_and_deduped() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("dictionary.txt"), "Yepgent\n\n  dimitri \nYEPGENT\n").unwrap();
        assert_eq!(dictionary_words(&dir.0), ["dimitri", "yepgent"]);
    }

    #[test]
    fn autocorrect_parses_like_the_correct_crate() {
        let dir = TempDir::new();
        std::fs::write(
            dir.0.join("autocorrect.txt"),
            "# typo -> fix\nTaht -> that\ndimitr->Dimitri\nsame -> SAME\nno arrow\n -> empty\nempty ->\nteh -> the\nteh -> The\n",
        )
        .unwrap();
        assert_eq!(
            autocorrect_pairs(&dir.0),
            [
                ("dimitr".to_string(), "Dimitri".to_string()),
                ("taht".to_string(), "that".to_string()),
                ("teh".to_string(), "The".to_string()),
            ]
        );
    }

    #[test]
    fn privacy_toml_matches_the_app_format() {
        let p = parse_privacy(
            "# comment = ignored\ndeny_exe = [\"1password.exe\", \"keepassxc.exe\"]\n\
             deny_title = [\"bank\", \"sign in\"]\nstore_context = true\n",
        );
        assert_eq!(
            p,
            PrivacyFile {
                present: true,
                deny_exe: vec!["1password.exe".into(), "keepassxc.exe".into()],
                deny_title: vec!["bank".into(), "sign in".into()],
                store_context: true,
            }
        );
        let empty = parse_privacy("deny_exe = []\n");
        assert!(empty.present && empty.deny_exe.is_empty() && !empty.store_context);
    }

    #[test]
    fn privacy_log_counts_by_reason_and_window() {
        let text = "{\"ts\":1000,\"reason\":\"card number\"}\n\
                    {\"ts\":5000,\"reason\":\"card number\"}\n\
                    {\"ts\":6000,\"reason\":\"long number\",\"app\":\"x.exe\"}\n\
                    {\"reason\":\"app notepad.exe\"}\n\
                    {\"ts\":7000,\"reason\":\"title matches \\\"bank\\\"\"}\n\
                    {\"ts\":7000}\n\
                    not json\n\
                    [1,2]\n";
        let all = count_privacy_log(text, 0);
        assert_eq!(all.by_reason["card number"], 2);
        assert_eq!(all.by_reason["unknown"], 1);
        assert_eq!(all.sentences_blocked(), 2 + 1 + 1);
        assert_eq!(all.app_blocks(), 2);

        let recent = count_privacy_log(text, 5000);
        assert_eq!(recent.by_reason["card number"], 1);
        assert_eq!(recent.by_reason["app notepad.exe"], 1, "lines without ts always count");
        assert_eq!(recent.sentences_blocked(), 3); // card number, long number, unknown
    }

    #[test]
    fn privacy_log_reads_only_the_tail_of_a_huge_file() {
        let dir = TempDir::new();
        let line = "{\"ts\":1,\"reason\":\"old\"}\n";
        let mut text = line.repeat((MAX_FILE_BYTES as usize / line.len()) + 100);
        text.push_str("{\"ts\":2,\"reason\":\"new\"}\n");
        std::fs::write(dir.0.join("privacy_log.jsonl"), text).unwrap();
        let log = privacy_log(&dir.0, 0);
        assert!(log.present);
        assert_eq!(log.by_reason["new"], 1);
        assert!(log.by_reason["old"] > 1000);
    }
}
