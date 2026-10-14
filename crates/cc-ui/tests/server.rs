//! End-to-end: a real server on a real socket over a temp profile.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cc_store::{Action, NewEvent, Outcome, Source, Store};
use cc_ui::{ModelInfo, UiCommand, UiServer, UiSettings, UiState};
use crossbeam_channel::unbounded;
use serde_json::Value;

struct Profile(PathBuf);

impl Profile {
    fn new() -> Profile {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "cc-ui-it-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Profile(p)
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Response {
    status: u16,
    headers: String,
    body: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers.lines().any(|l| l.to_ascii_lowercase().starts_with(&format!("{}:", name.to_ascii_lowercase())))
    }
}

/// A minimal HTTP/1.1 client: one request per connection.
fn request(port: u16, method: &str, target: &str, extra: &[(&str, &str)], body: Option<&str>) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut head = format!("{method} {target} HTTP/1.1\r\nConnection: close\r\n");
    if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        head.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    if let Some(b) = body {
        stream.write_all(b.as_bytes()).unwrap();
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Response { status, headers: head.to_string(), body: body.to_string() }
}

fn token_of(url: &str) -> String {
    let after = url.split_once("?t=").expect("url carries the token").1;
    after.split('#').next().unwrap().to_string()
}

fn log(store: &Store, source: Source, action: Action, original: &str, replacement: &str, outcome: Outcome) {
    let id = store
        .log(&NewEvent {
            app_exe: "notepad.exe".into(),
            source,
            rule: if source == Source::LlmComplete { "complete" } else { "spell" }.into(),
            severity: "S1".into(),
            confidence: 0.4,
            context_before: None,
            original: original.into(),
            replacement: replacement.into(),
            action,
            latency_ms: 90,
        })
        .unwrap();
    store.set_outcome(id, outcome).unwrap();
}

#[test]
fn page_and_api_over_a_real_socket() {
    let profile = Profile::new();
    let db = profile.0.join("events.db");
    let store = Store::open(&db).unwrap();
    log(&store, Source::Harper, Action::Auto, "teh", "the", Outcome::Kept);
    log(&store, Source::Harper, Action::Auto, "adn", "and", Outcome::Reverted);
    log(&store, Source::LlmComplete, Action::Suggested, "", "hello world", Outcome::Accepted);

    let state = Arc::new(UiState::default());
    *state.settings.lock().unwrap() = UiSettings {
        enabled: true,
        auto_fix_level: "S1".into(),
        model: "gemma-3-1b-pt".into(),
        min_confidence: 0.12,
        models: vec![ModelInfo {
            id: "gemma-3-1b-pt".into(),
            label: "Gemma 3 1B".into(),
            origin: "Google".into(),
            installed: true,
        }],
        profile_dir: profile.0.display().to_string(),
        ..UiSettings::default()
    };
    let (tx, rx) = unbounded();
    let server = UiServer::start(&db, &profile.0, state.clone(), tx).unwrap();
    let port = server.port();
    assert_ne!(port, 0);

    // Addresses carry the port, the token and the route.
    let dash = server.url("dashboard");
    assert!(dash.starts_with(&format!("http://127.0.0.1:{port}/?t=")), "{dash}");
    assert!(dash.ends_with("#/dashboard"), "{dash}");
    assert!(server.url("flyout").ends_with("#/flyout"));
    let token = token_of(&dash);
    assert_eq!(token.len(), 64);
    assert_eq!(token_of(&server.url("flyout")), token);

    // The page: only with the token in the query.
    let r = request(port, "GET", "/", &[], None);
    assert_eq!(r.status, 403);
    assert!(!r.body.contains("id=\"app\""));
    let r = request(port, "GET", "/?t=nope", &[], None);
    assert_eq!(r.status, 403);
    let r = request(port, "GET", &format!("/?t={token}"), &[], None);
    assert_eq!(r.status, 200, "{}", r.headers);
    assert!(r.body.contains("id=\"app\""), "{}", r.body);
    assert!(r.headers.to_ascii_lowercase().contains("text/html"));
    assert!(r.has_header("Content-Security-Policy"));
    assert!(!r.has_header("Access-Control-Allow-Origin"), "no CORS, ever");
    assert_eq!(request(port, "GET", "/app.js", &[], None).status, 200);
    assert_eq!(request(port, "GET", "/app.css", &[], None).status, 200);

    // The API: only with the header.
    assert_eq!(request(port, "GET", "/api/metrics", &[], None).status, 401);
    assert_eq!(request(port, "GET", "/api/metrics", &[("X-Token", "nope")], None).status, 401);
    assert_eq!(request(port, "GET", &format!("/api/metrics?t={token}"), &[], None).status, 401);
    let auth = [("X-Token", token.as_str())];
    let r = request(port, "GET", "/api/metrics", &auth, None);
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.headers.to_ascii_lowercase().contains("application/json"));
    assert!(!r.has_header("Access-Control-Allow-Origin"));
    let m = r.json();
    assert_eq!(m["events"], 3);
    assert_eq!(m["fixes_kept"], 1);
    assert_eq!(m["fixes_reverted"], 1);
    assert_eq!(m["completions_accepted"], 1);
    assert_eq!(m["chars_saved"], 3 + 11);

    let r = request(port, "GET", "/api/series?days=7&tz=0", &auth, None);
    assert_eq!(r.status, 200);
    let rows = r.json();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 7);
    let today = rows.last().unwrap();
    assert_eq!((today["shown"].as_u64(), today["accepted"].as_u64()), (Some(1), Some(1)));
    assert_eq!((today["fixes_kept"].as_u64(), today["fixes_reverted"].as_u64()), (Some(1), Some(1)));
    assert_eq!(today["chars_saved"], 14);
    assert!(rows.iter().all(|r| r["date"].as_str().is_some_and(|d| d.len() == 10)));

    // A browser on another site, or a rebinding host name, gets nothing even with the token.
    let r = request(port, "GET", "/api/metrics", &[("X-Token", token.as_str()), ("Origin", "https://evil.example")], None);
    assert_eq!(r.status, 403);
    let r = request(port, "GET", "/api/metrics", &[("X-Token", token.as_str()), ("Host", "evil.example")], None);
    assert_eq!(r.status, 403);
    // A CORS preflight is refused and carries no permission headers.
    let r = request(
        port,
        "OPTIONS",
        "/api/settings",
        &[("Origin", "http://127.0.0.1:1"), ("Access-Control-Request-Method", "POST")],
        None,
    );
    assert_eq!(r.status, 403);
    assert!(!r.has_header("Access-Control-Allow-Origin"));

    // Settings: GET reflects the state; POST queues commands and updates it optimistically.
    let s = request(port, "GET", "/api/settings", &auth, None).json();
    assert_eq!(s["model"], "gemma-3-1b-pt");
    assert_eq!(s["min_confidence"], 0.12);
    let origin = format!("http://127.0.0.1:{port}");
    let same_origin = [("X-Token", token.as_str()), ("Origin", origin.as_str())];
    let r = request(port, "POST", "/api/settings", &same_origin, Some(r#"{"auto_fix_level":"S2","min_confidence":0.2}"#));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["auto_fix_level"], "S2");
    assert_eq!(state.settings.lock().unwrap().auto_fix_level, "S2");
    let mut got = Vec::new();
    while let Ok(c) = rx.recv_timeout(Duration::from_millis(500)) {
        got.push(c);
    }
    assert!(got.contains(&UiCommand::SetAutoFixLevel("S2".into())), "{got:?}");
    assert!(got.contains(&UiCommand::SetMinConfidence(0.2)), "{got:?}");
    let r = request(port, "POST", "/api/settings", &same_origin, Some(r#"{"min_confidence":2}"#));
    assert_eq!(r.status, 400);
    assert!(rx.try_recv().is_err());

    // Words go out as commands.
    let r = request(port, "POST", "/api/words", &auth, Some(r#"{"add":"yepgent"}"#));
    assert_eq!(r.status, 200);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), UiCommand::AddDictionaryWord("yepgent".into()));

    // And the server stops with the handle.
    drop(server);
    let mut refused = false;
    for _ in 0..50 {
        if TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(), Duration::from_millis(200)).is_err() {
            refused = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(refused, "the port should close after the server is dropped");
}

#[test]
fn starts_on_a_fresh_profile_without_an_event_log() {
    let profile = Profile::new();
    let db = profile.0.join("nested").join("events.db");
    let (tx, _rx) = unbounded();
    let server = UiServer::start(&db, &profile.0, Arc::new(UiState::default()), tx).unwrap();
    assert!(db.exists(), "a read-only connection needs the file to exist");
    let token = token_of(&server.url("dashboard"));
    let auth = [("X-Token", token.as_str())];
    let m = request(server.port(), "GET", "/api/metrics", &auth, None).json();
    assert_eq!(m["events"], 0);
    let rows = request(server.port(), "GET", "/api/series?days=30", &auth, None).json();
    assert_eq!(rows.as_array().unwrap().len(), 30);
    assert_eq!(request(server.port(), "GET", "/api/context", &auth, None).json(), Value::Null);
    assert_eq!(request(server.port(), "GET", "/api/events", &auth, None).json(), serde_json::json!([]));
}

#[test]
fn two_servers_get_different_ports_and_tokens() {
    let profile = Profile::new();
    let db = profile.0.join("events.db");
    let start = || {
        let (tx, _rx) = unbounded();
        UiServer::start(&db, &profile.0, Arc::new(UiState::default()), tx).unwrap()
    };
    let (a, b) = (start(), start());
    assert_ne!(a.port(), b.port());
    assert_ne!(token_of(&a.url("flyout")), token_of(&b.url("flyout")));
    // One server's token does not open the other.
    let ta = token_of(&a.url("flyout"));
    assert_eq!(request(b.port(), "GET", &format!("/?t={ta}"), &[], None).status, 403);
}
