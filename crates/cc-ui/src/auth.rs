//! The per-run access token and the request-origin checks.

use anyhow::anyhow;

/// 32 random bytes from the OS, hex encoded.
pub(crate) fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("no OS randomness for the UI token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Does `given` equal `expected`? Compares in constant time; a missing or empty token never
/// matches.
pub(crate) fn token_matches(expected: &str, given: Option<&str>) -> bool {
    let Some(given) = given else { return false };
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Only our own address is a valid `Host`; anything else is a DNS-rebinding attempt.
pub(crate) fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else { return false };
    host.eq_ignore_ascii_case(&format!("127.0.0.1:{port}"))
        || host.eq_ignore_ascii_case(&format!("localhost:{port}"))
}

/// A request without `Origin` is fine (navigation, same-origin GET); one with it must come
/// from this server's own pages.
pub(crate) fn origin_allowed(origin: Option<&str>, port: u16) -> bool {
    match origin {
        None => true,
        Some(o) => {
            o.eq_ignore_ascii_case(&format!("http://127.0.0.1:{port}"))
                || o.eq_ignore_ascii_case(&format!("http://localhost:{port}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex_of_the_right_length() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn token_check_accepts_only_the_exact_token() {
        let t = "0123456789abcdef";
        assert!(token_matches(t, Some("0123456789abcdef")));
        assert!(!token_matches(t, None));
        assert!(!token_matches(t, Some("")));
        assert!(!token_matches(t, Some("0123456789abcdee")));
        assert!(!token_matches(t, Some("0123456789abcde")));
        assert!(!token_matches(t, Some("0123456789abcdef0")));
        assert!(!token_matches(t, Some("0123456789ABCDEF")));
        // An empty expected token (never produced by `new_token`) must not match anything.
        assert!(!token_matches("", Some("")));
    }

    #[test]
    fn host_must_be_this_server() {
        assert!(host_allowed(Some("127.0.0.1:5000"), 5000));
        assert!(host_allowed(Some("localhost:5000"), 5000));
        assert!(host_allowed(Some("LOCALHOST:5000"), 5000));
        assert!(!host_allowed(Some("127.0.0.1:5001"), 5000));
        assert!(!host_allowed(Some("evil.example:5000"), 5000));
        assert!(!host_allowed(Some("127.0.0.1"), 5000));
        assert!(!host_allowed(None, 5000));
    }

    #[test]
    fn origin_must_be_this_server_when_present() {
        assert!(origin_allowed(None, 5000));
        assert!(origin_allowed(Some("http://127.0.0.1:5000"), 5000));
        assert!(origin_allowed(Some("http://localhost:5000"), 5000));
        assert!(!origin_allowed(Some("http://127.0.0.1:5001"), 5000));
        assert!(!origin_allowed(Some("https://evil.example"), 5000));
        assert!(!origin_allowed(Some("null"), 5000));
    }
}
