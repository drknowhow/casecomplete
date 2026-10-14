//! Handler-level tests: requests go through `router::route` against a temp profile, no
//! sockets involved (the real-socket path is covered by `tests/server.rs`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use cc_store::{Action, NewEvent, Outcome, Source, Store};
use crossbeam_channel::{unbounded, Receiver};
use serde_json::{json, Value};

use crate::http::Req;
use crate::{router, ContextSnapshot, Ctx, ModelInfo, UiCommand, UiSettings, UiState};

const PORT: u16 = 4242;
const TOKEN: &str = "t0k3n-for-tests";

struct Env {
    dir: PathBuf,
    ctx: Ctx,
    rx: Receiver<UiCommand>,
    store: Store,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn env() -> Env {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "cc-ui-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("events.db");
    let store = Store::open(&db).unwrap();
    let state = Arc::new(UiState::default());
    *crate::lock(&state.settings) = UiSettings {
        enabled: true,
        auto_fix_level: "S1".into(),
        model: "gemma-3-1b-pt".into(),
        min_confidence: 0.12,
        models: vec![
            ModelInfo {
                id: "gemma-3-1b-pt".into(),
                label: "Gemma 3 1B".into(),
                origin: "Google".into(),
                installed: true,
            },
            ModelInfo { id: "qwen2.5-0.5b".into(), label: "Qwen 2.5 0.5B".into(), origin: "Alibaba".into(), installed: false },
        ],
        profile_dir: dir.display().to_string(),
        ..UiSettings::default()
    };
    let (tx, rx) = unbounded();
    let ctx = Ctx { db, profile: dir.clone(), state, commands: tx, token: TOKEN.into(), port: PORT };
    Env { dir, ctx, rx, store }
}

impl Env {
    fn call(&self, method: &str, target: &str, body: Option<&str>) -> (u16, Value) {
        let mut req = Req::test(method, target, PORT, Some(TOKEN));
        req.body = body.unwrap_or("").as_bytes().to_vec();
        let reply = router::route(&self.ctx, &req);
        let value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        (reply.status, value)
    }

    fn get(&self, target: &str) -> Value {
        let (status, v) = self.call("GET", target, None);
        assert_eq!(status, 200, "GET {target}: {v}");
        v
    }

    fn commands(&self) -> Vec<UiCommand> {
        self.rx.try_iter().collect()
    }

    fn settings(&self) -> UiSettings {
        crate::lock(&self.ctx.state.settings).clone()
    }

    #[allow(clippy::too_many_arguments)]
    fn log(&self, app: &str, source: Source, action: Action, original: &str, replacement: &str, conf: f32, latency: u32, outcome: Option<Outcome>) {
        let id = self
            .store
            .log(&NewEvent {
                app_exe: app.into(),
                source,
                rule: if source == Source::LlmComplete { "complete" } else { "spell" }.into(),
                severity: "S1".into(),
                confidence: conf,
                context_before: None,
                original: original.into(),
                replacement: replacement.into(),
                action,
                latency_ms: latency,
            })
            .unwrap();
        if let Some(o) = outcome {
            self.store.set_outcome(id, o).unwrap();
        }
    }
}

#[test]
fn page_needs_the_token_in_the_query() {
    let e = env();
    let page = |target: &str| {
        let req = Req::test("GET", target, PORT, None);
        router::route(&e.ctx, &req)
    };
    for bad in ["/", "/?t=", "/?t=wrong", "/?t=t0k3n-for-test", "/index.html"] {
        let r = page(bad);
        assert_eq!(r.status, 403, "{bad}");
        assert!(r.is_page);
        assert!(!String::from_utf8_lossy(&r.body).contains("id=\"app\""), "{bad} leaked the app");
    }
    for good in [format!("/?t={TOKEN}"), format!("/?x=1&t={TOKEN}"), format!("/index.html?t={TOKEN}")] {
        let r = page(&good);
        assert_eq!(r.status, 200, "{good}");
        assert!(r.is_page && r.content_type.starts_with("text/html"));
        assert!(String::from_utf8_lossy(&r.body).contains("id=\"app\""));
    }
}

#[test]
fn static_assets_are_public_but_typed() {
    let e = env();
    let js = router::route(&e.ctx, &Req::test("GET", "/app.js", PORT, None));
    assert_eq!((js.status, js.content_type), (200, "application/javascript; charset=utf-8"));
    let css = router::route(&e.ctx, &Req::test("GET", "/app.css", PORT, None));
    assert_eq!((css.status, css.content_type), (200, "text/css; charset=utf-8"));
}

#[test]
fn api_needs_the_x_token_header() {
    let e = env();
    let status = |target: &str, token: Option<&str>| {
        router::route(&e.ctx, &Req::test("GET", target, PORT, token)).status
    };
    assert_eq!(status("/api/metrics", None), 401);
    assert_eq!(status("/api/metrics", Some("")), 401);
    assert_eq!(status("/api/metrics", Some("nope")), 401);
    // The query-string token is for the page only; the API wants the header.
    assert_eq!(status(&format!("/api/metrics?t={TOKEN}"), None), 401);
    assert_eq!(status("/api/metrics", Some(TOKEN)), 200);
    // Even unknown API paths do not reveal anything without the token.
    assert_eq!(status("/api/nope", None), 401);
    assert_eq!(status("/api/nope", Some(TOKEN)), 404);
}

#[test]
fn foreign_host_or_origin_is_refused_even_with_the_token() {
    let e = env();
    let mut req = Req::test("GET", "/api/metrics", PORT, Some(TOKEN));
    assert_eq!(router::route(&e.ctx, &req).status, 200);
    req.host = Some("evil.example:4242".into());
    assert_eq!(router::route(&e.ctx, &req).status, 403);
    req.host = Some(format!("127.0.0.1:{PORT}"));
    req.origin = Some("https://evil.example".into());
    assert_eq!(router::route(&e.ctx, &req).status, 403);
    req.origin = Some(format!("http://127.0.0.1:{PORT}"));
    assert_eq!(router::route(&e.ctx, &req).status, 200);
    req.host = None;
    assert_eq!(router::route(&e.ctx, &req).status, 403);
}

#[test]
fn unknown_paths_and_wrong_methods() {
    let e = env();
    let page = |m: &str, t: &str| router::route(&e.ctx, &Req::test(m, t, PORT, Some(TOKEN))).status;
    assert_eq!(page("GET", "/nope"), 404);
    assert_eq!(page("POST", "/"), 405);
    assert_eq!(page("GET", "/api/nope"), 404);
    assert_eq!(page("DELETE", "/api/metrics"), 405);
    assert_eq!(page("GET", "/api/quit"), 405);
    assert_eq!(page("PUT", "/api/settings"), 405);
    let mut big = Req::test("POST", "/api/settings", PORT, Some(TOKEN));
    big.too_large = true;
    assert_eq!(router::route(&e.ctx, &big).status, 413);
}

#[test]
fn read_endpoints_summarise_the_event_log() {
    let e = env();
    let (fix, comp) = (Source::Harper, Source::LlmComplete);
    e.log("notepad.exe", fix, Action::Auto, "teh", "the", 0.9, 4, Some(Outcome::Kept));
    e.log("notepad.exe", fix, Action::Auto, "adn", "and", 0.9, 4, Some(Outcome::Reverted));
    e.log("code.exe", comp, Action::Suggested, "", "hello world", 0.35, 120, Some(Outcome::Accepted));
    e.log("code.exe", comp, Action::Suggested, "", "foo", 0.75, 310, Some(Outcome::Ignored));

    let m = e.get("/api/metrics");
    assert_eq!(m["events"], 4);
    assert_eq!(m["fixes_kept"], 1);
    assert_eq!(m["fixes_reverted"], 1);
    assert_eq!(m["completions_shown"], 2);
    assert_eq!(m["completions_accepted"], 1);
    assert_eq!(m["chars_saved"], 3 + 11);
    assert_eq!(e.get("/api/metrics?since_ms=99999999999999")["events"], 0);
    assert_eq!(e.call("GET", "/api/metrics?since_ms=abc", None).0, 400);

    let s = e.get("/api/series?days=3&tz=0");
    let rows = s.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let today = &rows[2];
    assert_eq!(today["shown"], 2);
    assert_eq!(today["accepted"], 1);
    assert_eq!(today["fixes_kept"], 1);
    assert_eq!(today["fixes_reverted"], 1);
    assert_eq!(today["chars_saved"], 14);
    assert_eq!(rows[0]["shown"], 0);
    assert!(today["date"].as_str().unwrap().len() == 10);
    assert_eq!(e.get("/api/series").as_array().unwrap().len(), 30);
    assert_eq!(e.call("GET", "/api/series?tz=x", None).0, 400);
    assert_eq!(e.call("GET", "/api/series?days=x", None).0, 400);

    let l = e.get("/api/latency?days=7");
    assert_eq!(l["bucket_ms"], 20);
    assert_eq!(l["total"], 2);
    assert_eq!((l["p50_ms"].as_u64(), l["p95_ms"].as_u64()), (Some(120), Some(310)));
    let buckets = l["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 16);
    assert_eq!((buckets[6]["from"].as_u64(), buckets[6]["to"].as_u64(), buckets[6]["count"].as_u64()), (Some(120), Some(140), Some(1)));
    assert_eq!(buckets[15]["to"], Value::Null);
    assert_eq!(buckets[15]["count"], 1);

    let apps = e.get("/api/apps?days=7");
    let apps = apps.as_array().unwrap();
    assert_eq!(apps.len(), 2);
    assert_eq!(apps[0]["app_exe"], "notepad.exe");
    assert_eq!((apps[0]["fixes_kept"].as_u64(), apps[0]["fixes_reverted"].as_u64()), (Some(1), Some(1)));
    assert_eq!((apps[1]["shown"].as_u64(), apps[1]["accepted"].as_u64()), (Some(2), Some(1)));

    let conf = e.get("/api/confidence?days=7");
    let conf = conf.as_array().unwrap();
    assert_eq!(conf.len(), 10);
    assert_eq!((conf[3]["shown"].as_u64(), conf[3]["accepted"].as_u64()), (Some(1), Some(1)));
    assert_eq!(conf[7]["shown"], 1);
    assert_eq!((conf[3]["lo"].as_f64(), conf[3]["hi"].as_f64()), (Some(0.3), Some(0.4)));

    let all = e.get("/api/events");
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all[0]["replacement"], "foo");
    assert_eq!(all[0]["confidence"], 0.75);
    assert_eq!(e.get("/api/events?kind=fix").as_array().unwrap().len(), 2);
    assert_eq!(e.get("/api/events?kind=complete&limit=1").as_array().unwrap().len(), 1);
    assert_eq!(e.get("/api/events?limit=0").as_array().unwrap().len(), 1, "limit is clamped to at least 1");
    assert_eq!(e.call("GET", "/api/events?kind=zzz", None).0, 400);

    let rules = e.get("/api/rules");
    let rules = rules.as_array().unwrap();
    assert!(rules.iter().any(|r| r["rule"] == "spell" && r["auto_total"] == 2 && r["demoted"] == false));
}

#[test]
fn settings_round_trip_and_commands() {
    let e = env();
    let s = e.get("/api/settings");
    assert_eq!(s["enabled"], true);
    assert_eq!(s["min_confidence"], 0.12);
    assert_eq!(s["models"][1]["id"], "qwen2.5-0.5b");

    let (status, s) = e.call(
        "POST",
        "/api/settings",
        Some(r#"{"auto_fix_level":"s2","model":"qwen2.5-0.5b","min_confidence":0.25,"autostart":true,"enabled":false}"#),
    );
    assert_eq!(status, 200, "{s}");
    assert_eq!(s["auto_fix_level"], "S2");
    assert_eq!(s["model"], "qwen2.5-0.5b");
    assert_eq!(s["min_confidence"], 0.25);
    assert_eq!((s["autostart"].as_bool(), s["enabled"].as_bool()), (Some(true), Some(false)));
    let mut cmds = e.commands();
    assert_eq!(cmds.len(), 5);
    for want in [
        UiCommand::SetAutoFixLevel("S2".into()),
        UiCommand::SetModel("qwen2.5-0.5b".into()),
        UiCommand::SetMinConfidence(0.25),
        UiCommand::SetAutostart(true),
        UiCommand::SetEnabled(false),
    ] {
        let at = cmds.iter().position(|c| *c == want).unwrap_or_else(|| panic!("missing {want:?} in {cmds:?}"));
        cmds.remove(at);
    }
    let now = e.settings();
    assert_eq!((now.auto_fix_level.as_str(), now.model.as_str(), now.autostart, now.enabled), ("S2", "qwen2.5-0.5b", true, false));

    // Pausing sets a deadline; turning the app on clears it.
    let (status, s) = e.call("POST", "/api/settings", Some(r#"{"pause_minutes":10}"#));
    assert_eq!(status, 200);
    let until = s["paused_until_ms"].as_i64().unwrap();
    let ten = 10 * 60_000;
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!((until - now_ms - ten).abs() < 5_000, "{until} vs {now_ms}");
    assert_eq!(e.commands(), [UiCommand::PauseMinutes(10)]);
    let (_, s) = e.call("POST", "/api/settings", Some(r#"{"enabled":true}"#));
    assert_eq!(s["paused_until_ms"], Value::Null);
    assert_eq!(e.commands(), [UiCommand::SetEnabled(true)]);
    let (_, s) = e.call("POST", "/api/settings", Some(r#"{"pause_minutes":0}"#));
    assert_eq!(s["paused_until_ms"], Value::Null);

    // An empty body changes nothing and sends nothing.
    let before = e.commands().len();
    let (status, _) = e.call("POST", "/api/settings", Some("{}"));
    assert_eq!((status, before + e.commands().len()), (200, 1));
}

#[test]
fn settings_validation_rejects_without_side_effects() {
    let e = env();
    let before = serde_json::to_string(&e.get("/api/settings")).unwrap();
    for body in [
        r#"{"auto_fix_level":"S9"}"#,
        r#"{"auto_fix_level":3}"#,
        r#"{"model":"not-a-model"}"#,
        r#"{"model":""}"#,
        r#"{"min_confidence":1.5}"#,
        r#"{"min_confidence":-0.1}"#,
        r#"{"min_confidence":"high"}"#,
        r#"{"enabled":"yes"}"#,
        r#"{"autostart":1}"#,
        r#"{"pause_minutes":-5}"#,
        r#"{"pause_minutes":100000}"#,
        r#"{"pause_minutes":1.5}"#,
        r#"{"nonsense":true}"#,
        // One bad field poisons the whole request, including the valid one.
        r#"{"enabled":false,"min_confidence":7}"#,
        r#"[1,2]"#,
        "not json",
    ] {
        let (status, v) = e.call("POST", "/api/settings", Some(body));
        assert_eq!(status, 400, "{body}: {v}");
        assert!(v["error"].as_str().is_some_and(|m| !m.is_empty()), "{body}");
    }
    assert!(e.commands().is_empty());
    assert_eq!(serde_json::to_string(&e.get("/api/settings")).unwrap(), before);
}

#[test]
fn model_ids_are_free_form_when_the_app_lists_none() {
    let e = env();
    crate::lock(&e.ctx.state.settings).models.clear();
    assert_eq!(e.call("POST", "/api/settings", Some(r#"{"model":"anything-1.0"}"#)).0, 200);
    assert_eq!(e.call("POST", "/api/settings", Some(r#"{"model":"bad id; rm"}"#)).0, 400);
    assert_eq!(e.commands(), [UiCommand::SetModel("anything-1.0".into())]);
}

#[test]
fn word_and_autocorrect_edits_become_commands() {
    let e = env();
    for (method, path, body) in [
        ("POST", "/api/words", r#"{"add":"Yepgent"}"#),
        ("POST", "/api/words", r#"{"remove":"yepgent"}"#),
        ("POST", "/api/words", r#"{"action":"add","word":"dimitri"}"#),
        ("POST", "/api/words", r#"{"remove":true,"word":"old"}"#),
        ("DELETE", "/api/words", r#"{"word":"gone"}"#),
        ("POST", "/api/autocorrect", r#"{"typo":"taht","fix":"that"}"#),
        ("POST", "/api/autocorrect", r#"{"typo":"dimitr","fix":"Dimitri"}"#),
        ("DELETE", "/api/autocorrect", r#"{"typo":"taht"}"#),
        ("DELETE", "/api/autocorrect?typo=wierd", ""),
        ("POST", "/api/open-profile", ""),
        ("POST", "/api/quit", ""),
    ] {
        let (status, v) = e.call(method, path, Some(body));
        assert_eq!(status, 200, "{method} {path} {body}: {v}");
        assert_eq!(v["ok"], true);
    }
    assert_eq!(
        e.commands(),
        [
            UiCommand::AddDictionaryWord("Yepgent".into()),
            UiCommand::RemoveDictionaryWord("yepgent".into()),
            UiCommand::AddDictionaryWord("dimitri".into()),
            UiCommand::RemoveDictionaryWord("old".into()),
            UiCommand::RemoveDictionaryWord("gone".into()),
            UiCommand::SetAutocorrect("taht".into(), "that".into()),
            UiCommand::SetAutocorrect("dimitr".into(), "Dimitri".into()),
            UiCommand::RemoveAutocorrect("taht".into()),
            UiCommand::RemoveAutocorrect("wierd".into()),
            UiCommand::OpenProfileFolder,
            UiCommand::Quit,
        ]
    );
}

#[test]
fn word_and_autocorrect_validation() {
    let e = env();
    let long = "x".repeat(65);
    let long_word = format!(r#"{{"add":"{long}"}}"#);
    for (method, path, body) in [
        ("POST", "/api/words", "{}"),
        ("POST", "/api/words", r#"{"add":""}"#),
        ("POST", "/api/words", r#"{"add":"two words"}"#),
        ("POST", "/api/words", r#"{"add":"line\nbreak"}"#),
        ("POST", "/api/words", r#"{"add":5}"#),
        ("POST", "/api/words", long_word.as_str()),
        ("DELETE", "/api/words", "{}"),
        ("POST", "/api/autocorrect", r#"{"typo":"a b","fix":"ab"}"#),
        ("POST", "/api/autocorrect", r#"{"typo":"a->b","fix":"ab"}"#),
        ("POST", "/api/autocorrect", r#"{"typo":"teh"}"#),
        ("POST", "/api/autocorrect", r#"{"typo":"teh","fix":"  "}"#),
        ("POST", "/api/autocorrect", r#"{"typo":"The","fix":"the"}"#),
        ("POST", "/api/autocorrect", r#"{"typo":"teh","fix":"a\nb"}"#),
        ("DELETE", "/api/autocorrect", "{}"),
    ] {
        let (status, v) = e.call(method, path, Some(body));
        assert_eq!(status, 400, "{method} {path} {body}: {v}");
    }
    assert!(e.commands().is_empty());
}

#[test]
fn commands_fail_cleanly_when_the_app_is_gone() {
    let mut e = env();
    // Dropping the receiving end is what the orchestrator going away looks like.
    drop(std::mem::replace(&mut e.rx, unbounded().1));
    let (status, v) = e.call("POST", "/api/quit", None);
    assert_eq!(status, 503, "{v}");
    let before = e.settings().min_confidence;
    let (status, _) = e.call("POST", "/api/settings", Some(r#"{"min_confidence":0.9}"#));
    assert_eq!(status, 503);
    assert_eq!(e.settings().min_confidence, before, "state is not changed when the command was not sent");
}

#[test]
fn learned_and_privacy_read_the_profile_files() {
    let e = env();
    let l = e.get("/api/learned");
    assert_eq!((l["autocorrect_pairs"].as_u64(), l["dictionary_words"].as_u64(), l["rules_demoted"].as_u64()), (Some(0), Some(0), Some(0)));
    let p = e.get("/api/privacy");
    assert_eq!((p["log_present"].as_bool(), p["policy_present"].as_bool(), p["sentences_blocked"].as_u64()), (Some(false), Some(false), Some(0)));

    std::fs::write(e.dir.join("dictionary.txt"), "yepgent\ndimitri\n").unwrap();
    std::fs::write(e.dir.join("autocorrect.txt"), "# header\nteh -> the\ntaht -> that\n").unwrap();
    std::fs::write(
        e.dir.join("privacy.toml"),
        "deny_exe = [\"1password.exe\", \"keepassxc.exe\"]\ndeny_title = [\"bank\"]\nstore_context = false\n",
    )
    .unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    std::fs::write(
        e.dir.join("privacy_log.jsonl"),
        format!(
            "{{\"ts\":{now},\"reason\":\"card number\"}}\n{{\"ts\":{now},\"reason\":\"long number\"}}\n\
             {{\"ts\":{now},\"reason\":\"long number\"}}\n{{\"ts\":{now},\"reason\":\"app 1password.exe\"}}\n\
             {{\"ts\":1000,\"reason\":\"card number\"}}\n"
        ),
    )
    .unwrap();
    // Demote a rule so it shows up under "learned".
    for i in 0..25 {
        let id = e
            .store
            .log(&NewEvent {
                app_exe: "a.exe".into(),
                source: Source::Harper,
                rule: "bad_rule".into(),
                severity: "S1".into(),
                confidence: 0.9,
                context_before: None,
                original: "teh".into(),
                replacement: "the".into(),
                action: Action::Auto,
                latency_ms: 1,
            })
            .unwrap();
        e.store.set_outcome(id, if i < 10 { Outcome::Reverted } else { Outcome::Kept }).unwrap();
    }
    e.store.tune().unwrap();

    let l = e.get("/api/learned?days=30");
    assert_eq!(l["autocorrect_pairs"], 2);
    assert_eq!(l["dictionary_words"], 2);
    assert_eq!(l["rules_demoted"], 1);
    assert_eq!(l["demoted_rules"], json!(["bad_rule"]));
    assert_eq!(l["dictionary"], json!(["dimitri", "yepgent"]));
    assert_eq!(l["autocorrect"][0], json!({"typo": "taht", "fix": "that"}));

    let p = e.get("/api/privacy?days=30");
    assert_eq!(p["log_present"], true);
    assert_eq!(p["policy_present"], true);
    assert_eq!(p["sentences_blocked"], 3, "the 1970 entry is outside the window");
    assert_eq!(p["app_blocks"], 1);
    assert_eq!(p["by_reason"]["long number"], 2);
    assert_eq!(p["deny_exe"], json!(["1password.exe", "keepassxc.exe"]));
    assert_eq!(p["deny_title"], json!(["bank"]));
    assert_eq!(p["apps_kept_off"], 2);

    let rules = e.get("/api/rules");
    let bad = rules.as_array().unwrap().iter().find(|r| r["rule"] == "bad_rule").unwrap();
    assert_eq!(bad["demoted"], true);
}

#[test]
fn context_is_null_until_the_app_publishes_one() {
    let e = env();
    assert_eq!(e.get("/api/context"), Value::Null);
    *crate::lock(&e.ctx.state.last_context) = Some(ContextSnapshot {
        ts_ms: 1_700_000_000_000,
        app_exe: "code.exe".into(),
        style_lines: vec!["I have been stuck on it.".into()],
        prompt_tail: "want us to also add a setting that keeps the".into(),
        heal: String::new(),
        need_space: true,
        completion: " app running after a reboot.".into(),
        confidence: 0.34,
        tokens: 7,
        latency_ms: 131,
        prompt_ms: 6.5,
        outcome: Some("ignored".into()),
        privacy_block: None,
    });
    let c = e.get("/api/context");
    assert_eq!(c["app_exe"], "code.exe");
    assert_eq!(c["confidence"], 0.34);
    assert_eq!(c["prompt_ms"], 6.5);
    assert_eq!(c["need_space"], true);
    assert_eq!(c["style_lines"][0], "I have been stuck on it.");
    assert_eq!(c["privacy_block"], Value::Null);
}

#[test]
fn a_dead_event_log_is_a_503_not_a_crash() {
    let mut e = env();
    e.ctx.db = e.dir.join("missing").join("events.db");
    let (status, v) = e.call("GET", "/api/metrics", None);
    assert_eq!(status, 503, "{v}");
    // Settings and context do not depend on the log.
    assert_eq!(e.call("GET", "/api/settings", None).0, 200);
    assert_eq!(e.call("GET", "/api/context", None).0, 200);
}
