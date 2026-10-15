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
}

fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once("->")?;
            Some((k.trim().to_lowercase(), v.trim().to_string()))
        })
        .filter(|(k, v)| !k.is_empty() && !v.is_empty() && k != &v.to_lowercase())
        .collect()
}

impl AutocorrectList {
    /// Built-in seed plus `path` (`typo -> fix` per line); a missing file is fine.
    pub fn load(path: &Path) -> AutocorrectList {
        let user = std::fs::read_to_string(path).map(|t| parse(&t)).unwrap_or_default();
        AutocorrectList { seed: parse(SEED), user }
    }

    /// The fix for `word`, with the word's capitalisation carried over. User entries win.
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
        self.user.insert(typo.to_lowercase(), fix.to_string());
    }

    /// Removes a user entry and shadows a seed entry so it never fires again.
    pub fn remove(&mut self, typo: &str) {
        let key = typo.to_lowercase();
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
}
