//! Instant typo -> fix list: a built-in seed plus the profile's own entries, learned from
//! how the user corrects themself. Consulted before the grammar checker.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::apply_case;

const SEED: &str = include_str!("autocorrect_seed.txt");

#[derive(Debug, Clone, Default)]
pub struct AutocorrectList {
    seed: HashMap<String, String>,
    user: HashMap<String, String>,
    /// Loading the user file changed or dropped an entry, so the file is out of date.
    stale_file: bool,
}

/// `typo` split into the punctuation before its letters, the letters, and the punctuation
/// after them. None when it has no letters or digits.
fn split_punctuation(typo: &str) -> Option<(&str, &str, &str)> {
    let start = typo.find(char::is_alphanumeric)?;
    let (last, c) = typo.char_indices().rev().find(|(_, c)| c.is_alphanumeric())?;
    let end = last + c.len_utf8();
    Some((&typo[..start], &typo[start..end], &typo[end..]))
}

/// A `typo -> fix` pair keyed by the bare typo, with the punctuation around the typo also
/// taken off the fix: a learned `reeviews, -> reviews` would otherwise delete the comma.
fn normalize(typo: &str, fix: &str) -> Option<(String, String)> {
    let (lead, core, trail) = split_punctuation(typo.trim())?;
    let fix = fix.trim();
    let fix = fix.strip_prefix(lead).unwrap_or(fix);
    let fix = fix.strip_suffix(trail).unwrap_or(fix).trim();
    let key = core.to_lowercase();
    (!fix.is_empty() && key != fix.to_lowercase()).then(|| (key, fix.to_string()))
}

/// The entries of `text`, and whether normalising changed or dropped any of them.
fn parse(text: &str) -> (HashMap<String, String>, bool) {
    let mut entries = HashMap::new();
    let mut changed = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((typo, fix)) = line.split_once("->") else { continue };
        match normalize(typo, fix) {
            Some((key, clean)) => {
                changed |= key != typo.trim().to_lowercase() || clean != fix.trim();
                entries.insert(key, clean);
            }
            None => changed = true,
        }
    }
    (entries, changed)
}

impl AutocorrectList {
    /// Built-in seed plus `path` (`typo -> fix` per line); a missing file is fine.
    pub fn load(path: &Path) -> AutocorrectList {
        let (user, stale_file) = std::fs::read_to_string(path).map(|t| parse(&t)).unwrap_or_default();
        AutocorrectList { seed: parse(SEED).0, user, stale_file }
    }

    /// The file `load` read had entries it had to clean up; `save` brings it up to date.
    pub fn file_is_stale(&self) -> bool {
        self.stale_file
    }

    /// The fix for `word` (a bare word, no punctuation around it), with the word's
    /// capitalisation carried over. User entries win.
    pub fn lookup(&self, word: &str) -> Option<String> {
        let key = word.to_lowercase();
        let fix = self.user.get(&key).or_else(|| self.seed.get(&key))?;
        Some(apply_case(word, fix))
    }

    /// `word` has an entry the user made or taught, as opposed to a seeded one.
    pub fn is_user_entry(&self, word: &str) -> bool {
        self.user.contains_key(&word.to_lowercase())
    }

    pub fn add(&mut self, typo: &str, fix: &str) {
        if let Some((key, fix)) = normalize(typo, fix) {
            self.user.insert(key, fix);
        }
    }

    /// Removes a user entry and shadows a seed entry so it never fires again.
    pub fn remove(&mut self, typo: &str) {
        let Some((_, core, _)) = split_punctuation(typo.trim()) else { return };
        let key = core.to_lowercase();
        self.user.remove(&key);
        self.seed.remove(&key);
    }

    pub fn len(&self) -> usize {
        self.seed.len() + self.user.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Writes the user entries only.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut entries: Vec<_> = self.user.iter().collect();
        entries.sort();
        let mut out = String::from("# typo -> fix, one per line; learned from your own corrections\n");
        for (k, v) in entries {
            out.push_str(k);
            out.push_str(" -> ");
            out.push_str(v);
            out.push('\n');
        }
        std::fs::write(path, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_has_common_typos_and_keeps_case() {
        let list = AutocorrectList::load(Path::new("Z:/does/not/exist.txt"));
        assert!(list.len() > 200);
        assert_eq!(list.lookup("teh").as_deref(), Some("the"));
        assert_eq!(list.lookup("Teh").as_deref(), Some("The"));
        assert_eq!(list.lookup("tst").as_deref(), Some("test"));
        assert_eq!(list.lookup("ths").as_deref(), Some("this"));
        assert_eq!(list.lookup("hello"), None);
    }

    #[test]
    fn user_entries_override_seed_and_round_trip() {
        let dir = std::env::temp_dir().join(format!("cc-auto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("autocorrect.txt");
        let mut list = AutocorrectList::load(&path);
        list.add("ths", "thus");
        list.add("Dimitr", "Dimitri");
        list.save(&path).unwrap();
        let again = AutocorrectList::load(&path);
        assert_eq!(again.lookup("ths").as_deref(), Some("thus"));
        assert_eq!(again.lookup("dimitr").as_deref(), Some("Dimitri"));
        let mut again = again;
        again.remove("teh");
        assert_eq!(again.lookup("teh"), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn punctuation_around_a_learned_typo_is_dropped_from_both_sides() {
        let dir = std::env::temp_dir().join(format!("cc-auto-punct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("autocorrect.txt");
        std::fs::write(&path, "(iof -> (if\nreeviews, -> reviews\nomw -> on my way!\n").unwrap();
        let mut list = AutocorrectList::load(&path);
        assert!(list.file_is_stale());
        assert_eq!(list.lookup("iof").as_deref(), Some("if"));
        assert_eq!(list.lookup("reeviews").as_deref(), Some("reviews"));
        assert_eq!(list.lookup("omw").as_deref(), Some("on my way!"));
        list.add("\"Wrod,", "\"word,");
        assert_eq!(list.lookup("wrod").as_deref(), Some("word"));
        list.save(&path).unwrap();
        assert!(!AutocorrectList::load(&path).file_is_stale());
        list.remove("reeviews,");
        assert_eq!(list.lookup("reeviews"), None);
        std::fs::remove_dir_all(dir).ok();
    }
}
