//! Checks an auto-fix must pass before it is typed into someone's text, and the breaker that
//! stops auto-fix in a window where fixes keep going wrong.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Two undone fixes this close together in one window trip the breaker.
const UNDO_WINDOW: Duration = Duration::from_secs(15);
const UNDO_LIMIT: usize = 2;
pub const PAUSE: Duration = Duration::from_secs(60);
/// Characters compared by [`same_tail`].
const TAIL: usize = 32;

/// Keys that end a word for auto-fix. An apostrophe or hyphen continues the word
/// (`doesn't`, `e-mail`): fixing `doesn` at the apostrophe leaves `does't`.
pub fn closes_word(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"')
}

/// Why `original` -> `replacement` must not be typed, if it must not. `whole_word`: the
/// shadow buffer saw where `original` starts, so the deletion cannot reach into an unseen
/// part of the word. `is_word` says whether a token is spelled right.
pub fn reject(original: &str, replacement: &str, whole_word: bool, is_word: impl Fn(&str) -> bool) -> Option<&'static str> {
    if !whole_word {
        return Some("start of the word not seen");
    }
    if replacement.split_whitespace().any(|token| !is_word(token)) {
        return Some("result is not a known word");
    }
    let letters = |s: &str| s.chars().filter(|c| c.is_alphabetic()).count();
    if letters(replacement) >= 2 && !replacement.chars().any(char::is_lowercase) && original.chars().any(char::is_lowercase) {
        return Some("would turn a word into capitals");
    }
    None
}

/// Whether `field` ends with the end of `shadow`. Word breaks count, so a swallowed space
/// (`previousthe`) is caught; what apps rewrite on their own is ignored: case, smart quotes,
/// the kind and number of spaces, trailing spaces.
pub fn same_tail(field: &str, shadow: &str) -> bool {
    let (field, shadow) = (comparable(field), comparable(shadow));
    field.ends_with(&shadow[shadow.len().saturating_sub(TAIL)..])
}

fn comparable(s: &str) -> Vec<char> {
    let mut out = Vec::new();
    for c in s.trim_end().chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            c if c.is_whitespace() => ' ',
            c => c,
        };
        if c == ' ' && out.last() == Some(&' ') {
            continue;
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// Pauses auto-fix per window after fixes are undone in quick succession or a fix check
/// fails: both mean the shadow buffer and the field disagree, and further fixes would
/// compound the damage.
#[derive(Default)]
pub struct Breaker {
    undone: HashMap<isize, Vec<Instant>>,
    paused_until: HashMap<isize, Instant>,
}

impl Breaker {
    /// Records an undone fix; true when that trips the breaker.
    pub fn undone(&mut self, hwnd: isize, now: Instant) -> bool {
        let recent = self.undone.entry(hwnd).or_default();
        recent.retain(|t| now.duration_since(*t) < UNDO_WINDOW);
        recent.push(now);
        if recent.len() < UNDO_LIMIT {
            return false;
        }
        recent.clear();
        self.trip(hwnd, now);
        true
    }

    pub fn trip(&mut self, hwnd: isize, now: Instant) {
        self.paused_until.insert(hwnd, now + PAUSE);
    }

    pub fn paused(&self, hwnd: isize, now: Instant) -> bool {
        self.paused_until.get(&hwnd).is_some_and(|t| now < *t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(token: &str) -> bool {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        ["the", "does", "doesn't", "i", "a", "lot"].contains(&token.as_str())
    }

    #[test]
    fn apostrophes_and_hyphens_continue_a_word() {
        for c in ['\'', '\u{2019}', '-', '_', '/', '@'] {
            assert!(!closes_word(c), "{c:?}");
        }
        for c in [' ', '\n', '\t', '.', ',', '?', '!', ':', ';', ')', '"'] {
            assert!(closes_word(c), "{c:?}");
        }
    }

    #[test]
    fn reject_explains_unsafe_fixes() {
        assert_eq!(reject("teh", "the", true, known), None);
        assert_eq!(reject("alot", "a lot", true, known), None);
        assert_eq!(reject("erge", "edge", false, known), Some("start of the word not seen"));
        assert_eq!(reject("doesn", "does't", true, known), Some("result is not a known word"));
        assert_eq!(reject("ne", "NE", true, |_| true), Some("would turn a word into capitals"));
        assert_eq!(reject("i", "I", true, known), None);
        assert_eq!(reject("Teh", "The", true, known), None);
    }

    #[test]
    fn same_tail_ignores_what_apps_rewrite() {
        assert!(same_tail("He said it doesn't work. ", "it doesn't work. "));
        assert!(same_tail("it doesn\u{2019}t\u{a0}work", "it doesn't work"));
        assert!(same_tail("New sentence", "new sentence"));
        assert!(same_tail("anything", ""));
        assert!(same_tail("the end", "the  end "));
        assert!(same_tail("line one\r\nline two", "line one\nline two"));
        assert!(!same_tail("it does't work ", "it doesn't work "));
        assert!(!same_tail("medge ", "merge "));
        assert!(!same_tail("previousthe ", "previous the "));
    }

    #[test]
    fn breaker_trips_on_two_quick_undos_in_one_window() {
        let mut b = Breaker::default();
        let t0 = Instant::now();
        assert!(!b.undone(1, t0));
        assert!(!b.undone(2, t0), "another window");
        assert!(!b.undone(1, t0 + UNDO_WINDOW + Duration::from_secs(1)), "too far apart");
        assert!(b.undone(1, t0 + UNDO_WINDOW + Duration::from_secs(5)));
        let tripped = t0 + UNDO_WINDOW + Duration::from_secs(5);
        assert!(b.paused(1, tripped + Duration::from_secs(1)));
        assert!(!b.paused(2, tripped));
        assert!(!b.paused(1, tripped + PAUSE));
    }
}
