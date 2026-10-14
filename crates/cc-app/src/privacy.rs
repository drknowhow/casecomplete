//! Keeps sensitive text away from the model and the event log.
//!
//! Two layers: a pattern gate over the text about to be used (card numbers, SSNs, long
//! digit runs, secrets next to their labels, IBANs), and a profile denylist of apps and
//! window titles (banking, health portals, password managers) where the assistant stays
//! off entirely. Both fail closed: a hit means no completion, no fix, no log entry.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub deny_exe: Vec<String>,
    pub deny_title: Vec<String>,
    /// Store the sentence around a fix in events.db. Off by default.
    pub store_context: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            deny_exe: ["1password.exe", "keepassxc.exe", "bitwarden.exe", "casecomplete.exe"]
                .map(String::from)
                .to_vec(),
            deny_title: [
                "mychart", "epic", "cerner", "patient portal", "health", "medical", "pharmacy",
                "bank", "banking", "checkout", "payment", "billing", "credit card", "paypal",
                "venmo", "zelle", "chase", "wells fargo", "fidelity", "schwab", "vanguard",
                "coinbase", "robinhood", "irs", "turbotax", "ssa.gov", "sign in", "log in",
                "login", "password",
            ]
            .map(String::from)
            .to_vec(),
            store_context: false,
        }
    }
}

impl Policy {
    /// `privacy.toml` in the profile: `deny_exe = [...]`, `deny_title = [...]`,
    /// `store_context = false`. Lists in the file replace the defaults; a missing file or
    /// key keeps them. Written back with the defaults when absent so they are editable.
    pub fn load(path: &Path) -> Policy {
        let mut policy = Policy::default();
        match std::fs::read_to_string(path) {
            Ok(text) => policy.apply(&text),
            Err(_) => {
                let _ = std::fs::write(path, policy.to_toml());
            }
        }
        policy
    }

    fn apply(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "deny_exe" => self.deny_exe = parse_list(value),
                "deny_title" => self.deny_title = parse_list(value),
                "store_context" => self.store_context = value == "true",
                _ => {}
            }
        }
    }

    fn to_toml(&self) -> String {
        let list = |v: &[String]| {
            v.iter().map(|s| format!("\"{}\"", s.replace('"', "\\\""))).collect::<Vec<_>>().join(", ")
        };
        format!(
            "# Apps (exe names) and window-title fragments where CaseComplete stays off.\n\
             # Matching is case-insensitive; title fragments match anywhere in the title.\n\
             deny_exe = [{}]\n\
             deny_title = [{}]\n\
             # Store the sentence around a fix in events.db (never for completions).\n\
             store_context = {}\n",
            list(&self.deny_exe),
            list(&self.deny_title),
            self.store_context
        )
    }

    pub fn denies(&self, exe: &str, title: &str) -> Option<String> {
        let exe_l = exe.to_lowercase();
        if self.deny_exe.iter().any(|e| e.to_lowercase() == exe_l) {
            return Some(format!("app {exe}"));
        }
        let title_l = title.to_lowercase();
        self.deny_title
            .iter()
            .find(|t| !t.is_empty() && title_l.contains(&t.to_lowercase()))
            .map(|t| format!("title matches {t:?}"))
    }
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

/// Why `text` must not leave the keyboard, if anything in it looks like a secret.
pub fn sensitive(text: &str) -> Option<&'static str> {
    if has_card_number(text) {
        return Some("card number");
    }
    if has_ssn(text) {
        return Some("SSN");
    }
    if has_iban(text) {
        return Some("IBAN");
    }
    if has_long_digit_run(text) {
        return Some("long number");
    }
    if has_labelled_secret(text) {
        return Some("secret after its label");
    }
    None
}

fn digit_groups(text: &str) -> Vec<String> {
    // Digits that are contiguous or separated by single spaces/dashes form one group.
    let mut groups = Vec::new();
    let mut cur = String::new();
    let mut pending_sep = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
            pending_sep = false;
        } else if (c == ' ' || c == '-') && !cur.is_empty() && !pending_sep {
            pending_sep = true;
        } else {
            if !cur.is_empty() {
                groups.push(std::mem::take(&mut cur));
            }
            pending_sep = false;
        }
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}

fn luhn_ok(digits: &str) -> bool {
    let mut sum = 0;
    let mut double = false;
    for c in digits.chars().rev() {
        let mut d = c.to_digit(10).unwrap_or(0);
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    sum % 10 == 0
}

fn has_card_number(text: &str) -> bool {
    digit_groups(text).iter().any(|g| (13..=19).contains(&g.len()) && luhn_ok(g))
}

fn has_long_digit_run(text: &str) -> bool {
    digit_groups(text).iter().any(|g| g.len() >= 9)
}

fn has_ssn(text: &str) -> bool {
    let b = text.as_bytes();
    (0..b.len().saturating_sub(10)).any(|i| {
        let s = &b[i..i + 11];
        s[3] == b'-'
            && s[6] == b'-'
            && s.iter().enumerate().all(|(j, c)| if j == 3 || j == 6 { true } else { c.is_ascii_digit() })
            && (i == 0 || !b[i - 1].is_ascii_digit())
            && (i + 11 >= b.len() || !b[i + 11].is_ascii_digit())
    })
}

fn has_iban(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        w.len() >= 15
            && w.len() <= 34
            && w[..2].chars().all(|c| c.is_ascii_uppercase())
            && w[2..4].chars().all(|c| c.is_ascii_digit())
            && w[4..].chars().any(|c| c.is_ascii_digit())
    })
}

fn has_labelled_secret(text: &str) -> bool {
    const LABELS: [&str; 10] =
        ["password", "passwd", "pwd", "passcode", "pin", "cvv", "cvc", "api key", "secret", "token"];
    let lower = text.to_lowercase();
    LABELS.iter().any(|label| {
        let mut from = 0;
        while let Some(i) = lower[from..].find(label) {
            let after = &lower[from + i + label.len()..];
            let after = after.trim_start();
            if after.starts_with([':', '=']) {
                let value = after[1..].trim_start();
                if value.chars().take_while(|c| !c.is_whitespace()).count() >= 3 {
                    return true;
                }
            }
            from += i + label.len();
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_numbers_are_caught_with_and_without_separators() {
        assert_eq!(sensitive("pay with 4111 1111 1111 1111 please"), Some("card number"));
        assert_eq!(sensitive("card 4111111111111111"), Some("card number"));
        assert_eq!(sensitive("5500-0000-0000-0004 exp 12/28"), Some("card number"));
    }

    #[test]
    fn ssn_and_iban_and_long_numbers() {
        assert_eq!(sensitive("my ssn is 123-45-6789 ok"), Some("SSN"));
        assert_eq!(sensitive("send to DE89370400440532013000 today"), Some("IBAN"));
        assert_eq!(sensitive("account 987654321 is mine"), Some("long number"));
    }

    #[test]
    fn labelled_secrets() {
        assert_eq!(sensitive("password: hunter22"), Some("secret after its label"));
        assert_eq!(sensitive("the CVV=123 on the back"), Some("secret after its label"));
        assert_eq!(sensitive("my password is strong"), None);
    }

    #[test]
    fn ordinary_text_passes() {
        assert_eq!(sensitive("I want us to create a system that allows me to type faster"), None);
        assert_eq!(sensitive("meet at 10:30 on 2026-10-14, room 4"), None);
        assert_eq!(sensitive("call 555-0100 after 5"), None);
    }

    #[test]
    fn policy_denies_by_exe_and_title_and_round_trips() {
        let p = Policy::default();
        assert!(p.denies("1Password.exe", "").is_some());
        assert!(p.denies("chrome.exe", "Chase - Sign In").is_some());
        assert!(p.denies("chrome.exe", "Gmail - Inbox").is_none());
        let mut q = Policy::default();
        q.apply("deny_exe = [\"x.exe\"]\ndeny_title = []\nstore_context = true\n");
        assert_eq!(q.deny_exe, vec!["x.exe".to_string()]);
        assert!(q.deny_title.is_empty());
        assert!(q.store_context);
        let mut r = Policy::default();
        r.apply(&p.to_toml());
        assert_eq!(r, p);
    }
}
