//! Checks an auto-fix must pass before it is typed into someone's text, and the breaker that
//! stops auto-fix in a window where fixes keep going wrong.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cc_correct::Severity;

/// Two undone fixes this close together in one window trip the breaker.
const UNDO_WINDOW: Duration = Duration::from_secs(15);
const UNDO_LIMIT: usize = 2;
pub const PAUSE: Duration = Duration::from_secs(60);
/// Characters compared by [`same_tail`].
const TAIL: usize = 32;
/// Characters the field may lag behind the shadow and still count as [`Tail::Behind`].
const LAG: usize = 8;

/// Keys that end a word for auto-fix. An apostrophe or hyphen continues the word
/// (`doesn't`, `e-mail`): fixing `doesn` at the apostrophe leaves `does't`.
pub fn closes_word(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"')
}

/// Whether a candidate may be typed without asking at auto-fix `level`. Bold (S2) adds only
/// doubled-letter spelling fixes, which S0 refuses on capitalised words because those look
/// like names (`Outllook` -> `Outlook`).
pub fn eligible(severity: &Severity, rule: &str, original: &str, replacement: &str, level: &Severity) -> bool {
    match severity {
        Severity::S0 | Severity::S1 => severity <= level,
        Severity::S2 => {
            *level >= Severity::S2 && rule == "spell" && cc_correct::is_doubled_letter_typo(original, replacement)
        }
        Severity::S3 => false,
    }
}

/// `s` without the punctuation around it: `(teh` is `teh`, `reviews,` is `reviews`.
pub fn word_core(s: &str) -> &str {
    s.trim_matches(|c: char| !c.is_alphanumeric())
}

/// The characters before and after the letters and digits of `s`.
fn outer_punctuation(s: &str) -> (&str, &str) {
    let not_alnum = |c: char| !c.is_alphanumeric();
    let lead = &s[..s.len() - s.trim_start_matches(not_alnum).len()];
    let trail = &s[s.trim_end_matches(not_alnum).len()..];
    (lead, trail)
}

/// Why `original` -> `replacement` must not be typed, if it must not. A fix replaces one
/// word with one word and keeps the punctuation around it; only an autocorrect entry
/// (`expands`) may stand for several words (`alot` -> `a lot`). `whole_word`: the shadow
/// buffer saw where `original` starts, so the deletion cannot reach into an unseen part of
/// the word. `is_word` says whether a token is spelled right.
pub fn reject(
    original: &str,
    replacement: &str,
    whole_word: bool,
    expands: bool,
    is_word: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    if original.trim().is_empty() || original.chars().any(char::is_whitespace) {
        return Some("changes more than one word");
    }
    if replacement.trim().is_empty() {
        return Some("would delete the word");
    }
    if replacement.chars().any(char::is_whitespace) && !expands {
        return Some("would split the word");
    }
    if outer_punctuation(original) != outer_punctuation(replacement) {
        return Some("would change the punctuation around the word");
    }
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

/// Field text as UIA gives it, with characters nobody typed turned into spaces one for one,
/// so lengths still match the shadow: U+FFFC stands for an embedded object (an image, a
/// mention chip), and browsers keep a typed space as a no-break space.
pub fn field_text(raw: &str) -> String {
    raw.chars().map(|c| if matches!(c, '\u{FFFC}' | '\u{A0}' | '\u{202F}') { ' ' } else { c }).collect()
}

/// How the end of the field's text relates to the end of the shadow buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum Tail {
    Same,
    /// The field is the shadow without its last few characters: UIA has not caught up.
    Behind,
    /// Something changed the text without the shadow seeing it.
    Different,
}

pub fn compare(field: &str, shadow: &str) -> Tail {
    if same_tail(field, shadow) {
        return Tail::Same;
    }
    let chars: Vec<char> = shadow.chars().collect();
    for k in 1..=LAG.min(chars.len()) {
        let older: String = chars[..chars.len() - k].iter().collect();
        // Too little left to tell a lagging field from a different one.
        if comparable(&older).len() < TAIL / 2 {
            break;
        }
        if same_tail(field, &older) {
            return Tail::Behind;
        }
    }
    Tail::Different
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
        assert_eq!(reject("teh", "the", true, false, known), None);
        assert_eq!(reject("alot", "a lot", true, true, known), None);
        assert_eq!(reject("alot", "a lot", true, false, known), Some("would split the word"));
        assert_eq!(reject("Every one", "Everyone", true, false, |_| true), Some("changes more than one word"));
        assert_eq!(reject("the the", "the", true, true, known), Some("changes more than one word"));
        assert_eq!(reject("teh", "", true, true, known), Some("would delete the word"));
        assert_eq!(
            reject("reeviews,", "reviews", true, true, |_| true),
            Some("would change the punctuation around the word")
        );
        assert_eq!(reject("(teh", "(the", true, false, known), None);
        assert_eq!(reject("erge", "edge", false, false, known), Some("start of the word not seen"));
        assert_eq!(reject("doesn", "does't", true, false, known), Some("result is not a known word"));
        assert_eq!(reject("ne", "NE", true, false, |_| true), Some("would turn a word into capitals"));
        assert_eq!(reject("i", "I", true, false, known), None);
        assert_eq!(reject("Teh", "The", true, false, known), None);
    }

    #[test]
    fn bold_adds_only_doubled_letter_spelling_fixes() {
        use Severity::*;
        assert!(eligible(&S0, "spell", "thhe", "the", &S0));
        assert!(!eligible(&S1, "sentence_capitalization", "this", "This", &S0));
        assert!(eligible(&S1, "sentence_capitalization", "this", "This", &S1));
        assert!(eligible(&S2, "spell", "Outllook", "Outlook", &S2));
        assert!(!eligible(&S2, "spell", "Outllook", "Outlook", &S1));
        assert!(!eligible(&S2, "spell", "pc", "pd", &S2));
        assert!(!eligible(&S2, "compound_nouns", "quick start", "quickstart", &S2));
        assert!(!eligible(&S3, "repeated_words", "the the", "the", &S2));
    }

    #[test]
    fn word_core_drops_surrounding_punctuation_only() {
        assert_eq!(word_core("(teh"), "teh");
        assert_eq!(word_core("reeviews,"), "reeviews");
        assert_eq!(word_core("don't"), "don't");
        assert_eq!(word_core("..."), "");
    }

    fn checker() -> &'static cc_correct::Checker {
        static CHECKER: std::sync::OnceLock<cc_correct::Checker> = std::sync::OnceLock::new();
        CHECKER.get_or_init(cc_correct::Checker::new)
    }

    /// What `on_word_boundary` would type after each word of `sentence` at `level`, as
    /// `(text so far, original, replacement)`.
    fn auto_fixes(sentence: &str, level: &Severity) -> Vec<(String, String, String)> {
        let dict = cc_correct::PersonalDictionary::default();
        let mut typed = String::new();
        let mut out = Vec::new();
        for word in sentence.split(' ') {
            if !typed.is_empty() {
                typed.push(' ');
            }
            typed.push_str(word);
            let n = typed.chars().count();
            for c in checker().check(&typed, &dict) {
                let at_caret = c.end == n && typed.ends_with(&c.original);
                if at_caret
                    && eligible(&c.severity, &c.rule, &c.original, &c.replacement, level)
                    && reject(&c.original, &c.replacement, true, false, |t| checker().is_word(t, &dict)).is_none()
                {
                    out.push((typed.clone(), c.original, c.replacement));
                }
            }
        }
        out
    }

    /// Sentences for which Harper offers to join, split or delete words.
    const JOIN_BAIT: &[&str] = &[
        "Every one of them came to the quick start meeting",
        "We should check out the new web site some time",
        "They had had enough and I know that that is true",
        "It is a good idea and also a cheap one",
        "The more friendly option is more better",
        "He should of known better alot of times",
        "This is a test of the the system",
        "Please make sure to double check the work",
        "The data base is on line now",
        "The kids played out side all day long",
        "Any one of these will do and some one who knows can help",
        "Some time ago some times I would go every where",
        "Every thing is fine with out a doubt",
        "Where as the old one failed there fore we left",
        "How ever you do it what ever happens is fine",
        "I need to up date the over load setting",
        "Our team is under way with the work",
        "It was a long while ago and we waited a while",
        "She is in deed right and in fact over joyed",
        "The book shelf and the note book are on the table",
        "I sent an e mail to my class mate yesterday",
        "We had a work shop on Monday and a lunch break",
    ];

    #[test]
    fn no_auto_fix_joins_splits_or_deletes_words() {
        for level in [Severity::S0, Severity::S1, Severity::S2] {
            for sentence in JOIN_BAIT {
                for (typed, original, replacement) in auto_fixes(sentence, &level) {
                    assert!(
                        original.split_whitespace().count() == 1 && replacement.split_whitespace().count() == 1,
                        "{level:?} after {typed:?}: {original:?} -> {replacement:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn typos_are_still_fixed_at_their_level() {
        let fixes = |s: &str, level: Severity| -> Vec<(String, String)> {
            auto_fixes(s, &level).into_iter().map(|(_, o, r)| (o, r)).collect()
        };
        let pair = |o: &str, r: &str| (o.to_string(), r.to_string());
        assert_eq!(fixes("I want thhe cat", Severity::S0), vec![pair("thhe", "the")]);
        assert_eq!(fixes("I said soo", Severity::S0), vec![pair("soo", "so")]);
        assert!(fixes("We use Outllook", Severity::S1).is_empty());
        assert_eq!(fixes("We use Outllook", Severity::S2), vec![pair("Outllook", "Outlook")]);
        assert!(fixes("My name is Dimitri", Severity::S2).is_empty());
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
    fn field_text_keeps_lengths_and_drops_what_was_never_typed() {
        assert_eq!(field_text("a\u{FFFC}b\u{A0}c"), "a b c");
        assert_eq!(field_text("plain").chars().count(), 5);
    }

    #[test]
    fn compare_tells_a_lagging_field_from_a_changed_one() {
        let shadow = "the quick brown fox jumps over the lazy dog";
        assert_eq!(compare("Once more: the quick brown fox jumps over the lazy dog", shadow), Tail::Same);
        assert_eq!(compare("the quick brown fox jumps over the lazy d", shadow), Tail::Behind);
        assert_eq!(compare("the quick brown fox jumps over the lazydog", shadow), Tail::Different);
        assert_eq!(compare("something else entirely", shadow), Tail::Different);
        assert_eq!(compare("anything", ""), Tail::Same, "an empty shadow knows nothing");
        assert_eq!(compare("ab", "abc"), Tail::Different, "too short to call it lag");
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
