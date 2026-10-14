//! The JSON API under `/api`. Reads open a fresh read-only connection per request; writes
//! never touch disk, they become [`UiCommand`]s for the orchestrator.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use cc_store::{EventKind, Store, LATENCY_BUCKETS, LATENCY_BUCKET_MS};
use serde_json::{json, Map, Value};

use crate::http::{Reply, Req};
use crate::{lock, profile, ContextSnapshot, Ctx, UiCommand, UiSettings};

const DAY_MS: i64 = 86_400_000;
const MAX_PAUSE_MINUTES: u64 = 24 * 60;
const MAX_WORD_CHARS: usize = 64;
const MAX_FIX_CHARS: usize = 128;
const MAX_APPS: usize = 20;
const KNOWN: &[&str] = &[
    "metrics", "series", "latency", "apps", "confidence", "events", "rules", "learned", "privacy",
    "settings", "context", "words", "autocorrect", "quit", "open-profile",
];

/// A failure with the HTTP status it maps to.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    fn new(status: u16, message: impl Into<String>) -> ApiError {
        ApiError { status, message: message.into() }
    }

    fn bad(message: impl Into<String>) -> ApiError {
        ApiError::new(400, message)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> ApiError {
        ApiError::new(500, format!("{e:#}"))
    }
}

type Res = Result<Value, ApiError>;

/// Dispatch `name` (the path after `/api/`).
pub(crate) fn handle(ctx: &Ctx, req: &Req, name: &str) -> Reply {
    let result = match (req.method.as_str(), name) {
        ("GET", "metrics") => metrics(ctx, req),
        ("GET", "series") => series(ctx, req),
        ("GET", "latency") => latency(ctx, req),
        ("GET", "apps") => apps(ctx, req),
        ("GET", "confidence") => confidence(ctx, req),
        ("GET", "events") => events(ctx, req),
        ("GET", "rules") => rules(ctx),
        ("GET", "learned") => learned(ctx, req),
        ("GET", "privacy") => privacy(ctx, req),
        ("GET", "settings") => Ok(settings_json(&lock(&ctx.state.settings))),
        ("POST", "settings") => post_settings(ctx, req),
        ("GET", "context") => Ok(context_json(lock(&ctx.state.last_context).as_ref())),
        ("POST", "words") => words(ctx, req, None),
        ("DELETE", "words") => words(ctx, req, Some(false)),
        ("POST", "autocorrect") => autocorrect_set(ctx, req),
        ("DELETE", "autocorrect") => autocorrect_remove(ctx, req),
        ("POST", "quit") => send(ctx, vec![UiCommand::Quit]).map(|()| json!({ "ok": true })),
        ("POST", "open-profile") => {
            send(ctx, vec![UiCommand::OpenProfileFolder]).map(|()| json!({ "ok": true }))
        }
        (_, n) if KNOWN.contains(&n) => Err(ApiError::new(405, "method not allowed")),
        _ => Err(ApiError::new(404, "no such endpoint")),
    };
    match result {
        Ok(v) => Reply::json(200, &v),
        Err(e) => Reply::error(e.status, &e.message),
    }
}

// ---- helpers --------------------------------------------------------------------------

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Three decimals, so f32 noise (0.11999999) never reaches the page.
fn r3(x: f64) -> f64 {
    if x.is_finite() {
        (x * 1000.0).round() / 1000.0
    } else {
        0.0
    }
}

fn open(ctx: &Ctx) -> Result<Store, ApiError> {
    Store::open_readonly(&ctx.db)
        .map_err(|e| ApiError::new(503, format!("event log unavailable: {e:#}")))
}

/// `?days=` (default `default`, clamped to 1..=366).
fn days_param(req: &Req, default: u32) -> Result<u32, ApiError> {
    match req.q("days") {
        None | Some("") => Ok(default),
        Some(s) => s
            .trim()
            .parse::<u32>()
            .map(|d| d.clamp(1, 366))
            .map_err(|_| ApiError::bad("days must be a whole number")),
    }
}

fn since_days(days: u32) -> i64 {
    now_ms() - i64::from(days) * DAY_MS
}

fn since_param(req: &Req) -> Result<Option<i64>, ApiError> {
    match req.q("since_ms") {
        None | Some("") => Ok(None),
        Some(s) => s
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(|_| ApiError::bad("since_ms must be unix milliseconds")),
    }
}

fn json_body(req: &Req) -> Result<Map<String, Value>, ApiError> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&req.body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(ApiError::bad("body must be a JSON object")),
        Err(e) => Err(ApiError::bad(format!("invalid JSON: {e}"))),
    }
}

/// Queue commands for the orchestrator; never blocks.
fn send(ctx: &Ctx, cmds: Vec<UiCommand>) -> Result<(), ApiError> {
    for cmd in cmds {
        ctx.commands
            .try_send(cmd)
            .map_err(|_| ApiError::new(503, "the app is not accepting commands right now"))?;
    }
    Ok(())
}

pub(crate) fn settings_json(s: &UiSettings) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("min_confidence".into(), json!(r3(f64::from(s.min_confidence))));
    }
    v
}

fn context_json(c: Option<&ContextSnapshot>) -> Value {
    let Some(c) = c else { return Value::Null };
    let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("confidence".into(), json!(r3(f64::from(c.confidence))));
        o.insert("prompt_ms".into(), json!(r3(f64::from(c.prompt_ms))));
    }
    v
}

// ---- event-log queries ----------------------------------------------------------------

fn metrics(ctx: &Ctx, req: &Req) -> Res {
    let since = since_param(req)?;
    let st = open(ctx)?;
    let m = st.metrics(since)?;
    let fixes_kept: u64 = st.by_app(since)?.iter().map(|a| a.fixes_kept).sum();
    Ok(json!({
        "since_ms": since,
        "events": m.events,
        "chars_saved": m.chars_saved,
        "fixes_auto": m.fixes_auto,
        "fixes_reverted": m.fixes_reverted,
        "fixes_kept": fixes_kept,
        "suggestions_accepted": m.suggestions_accepted,
        "completions_shown": m.completions_shown,
        "completions_accepted": m.completions_accepted,
        "p50_latency_ms": m.p50_latency_ms,
        "p95_latency_ms": m.p95_latency_ms,
    }))
}

fn series(ctx: &Ctx, req: &Req) -> Res {
    let days = days_param(req, 30)?;
    // Minutes east of UTC, as the browser knows its own zone (`-getTimezoneOffset()`).
    let tz = match req.q("tz") {
        None | Some("") => 0,
        Some(s) => s
            .trim()
            .parse::<i32>()
            .map(|t| t.clamp(-840, 840))
            .map_err(|_| ApiError::bad("tz must be minutes east of UTC"))?,
    };
    let rows = open(ctx)?.daily_series(days, tz)?;
    Ok(Value::Array(
        rows.into_iter()
            .map(|d| {
                json!({
                    "date": d.date,
                    "shown": d.shown,
                    "accepted": d.accepted,
                    "fixes_kept": d.fixes_kept,
                    "fixes_reverted": d.fixes_reverted,
                    "chars_saved": d.chars_saved,
                })
            })
            .collect(),
    ))
}

fn latency(ctx: &Ctx, req: &Req) -> Res {
    let days = days_param(req, 30)?;
    let h = open(ctx)?.latency_histogram(Some(since_days(days)))?;
    let buckets: Vec<Value> = h
        .counts
        .iter()
        .enumerate()
        .map(|(i, count)| {
            let from = i as u32 * LATENCY_BUCKET_MS;
            let to = if i + 1 == LATENCY_BUCKETS { Value::Null } else { json!(from + LATENCY_BUCKET_MS) };
            json!({ "from": from, "to": to, "count": count })
        })
        .collect();
    Ok(json!({
        "days": days,
        "bucket_ms": LATENCY_BUCKET_MS,
        "total": h.total,
        "p50_ms": h.p50_ms,
        "p95_ms": h.p95_ms,
        "buckets": buckets,
    }))
}

fn apps(ctx: &Ctx, req: &Req) -> Res {
    let days = days_param(req, 30)?;
    let rows = open(ctx)?.by_app(Some(since_days(days)))?;
    Ok(Value::Array(
        rows.into_iter()
            .take(MAX_APPS)
            .map(|a| {
                json!({
                    "app_exe": a.app_exe,
                    "fixes_kept": a.fixes_kept,
                    "fixes_reverted": a.fixes_reverted,
                    "shown": a.shown,
                    "accepted": a.accepted,
                })
            })
            .collect(),
    ))
}

fn confidence(ctx: &Ctx, req: &Req) -> Res {
    let days = days_param(req, 30)?;
    let rows = open(ctx)?.by_confidence(Some(since_days(days)))?;
    Ok(Value::Array(
        rows.into_iter()
            .map(|b| {
                json!({
                    "bucket": b.bucket,
                    "lo": f64::from(b.bucket) / 10.0,
                    "hi": f64::from(b.bucket + 1) / 10.0,
                    "shown": b.shown,
                    "accepted": b.accepted,
                })
            })
            .collect(),
    ))
}

fn events(ctx: &Ctx, req: &Req) -> Res {
    let limit = match req.q("limit") {
        None | Some("") => 50,
        Some(s) => s
            .trim()
            .parse::<usize>()
            .map(|n| n.clamp(1, 500))
            .map_err(|_| ApiError::bad("limit must be a whole number"))?,
    };
    let kind = match req.q("kind").unwrap_or("all") {
        "" | "all" => EventKind::All,
        "fix" => EventKind::Fix,
        "complete" => EventKind::Complete,
        other => return Err(ApiError::bad(format!("kind must be fix, complete or all, not {other:?}"))),
    };
    let rows = open(ctx)?.recent_events(limit, kind)?;
    Ok(Value::Array(
        rows.into_iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "ts": e.ts,
                    "app_exe": e.app_exe,
                    "source": e.source,
                    "rule": e.rule,
                    "severity": e.severity,
                    "confidence": r3(e.confidence),
                    "context_before": e.context_before,
                    "original": e.original,
                    "replacement": e.replacement,
                    "action": e.action,
                    "outcome": e.outcome,
                    "latency_ms": e.latency_ms,
                })
            })
            .collect(),
    ))
}

fn rules(ctx: &Ctx) -> Res {
    let st = open(ctx)?;
    let demoted: HashSet<String> = st.demoted_rules()?.into_iter().collect();
    Ok(Value::Array(
        st.rule_stats(0)?
            .into_iter()
            .map(|r| {
                json!({
                    "demoted": demoted.contains(&r.rule),
                    "rule": r.rule,
                    "auto_total": r.auto_total,
                    "auto_reverted": r.auto_reverted,
                    "suggested_total": r.suggested_total,
                    "suggested_accepted": r.suggested_accepted,
                })
            })
            .collect(),
    ))
}

// ---- profile files --------------------------------------------------------------------

/// `days` is accepted for symmetry with the other endpoints; the profile files carry no
/// timestamps, so the lists and counts are always current, not windowed.
fn learned(ctx: &Ctx, req: &Req) -> Res {
    let days = days_param(req, 30)?;
    let demoted = open(ctx)?.demoted_rules()?;
    let dictionary = profile::dictionary_words(&ctx.profile);
    let pairs = profile::autocorrect_pairs(&ctx.profile);
    Ok(json!({
        "days": days,
        "autocorrect_pairs": pairs.len(),
        "dictionary_words": dictionary.len(),
        "rules_demoted": demoted.len(),
        "autocorrect": pairs.iter().map(|(typo, fix)| json!({ "typo": typo, "fix": fix })).collect::<Vec<_>>(),
        "dictionary": dictionary,
        "demoted_rules": demoted,
    }))
}

fn privacy(ctx: &Ctx, req: &Req) -> Res {
    let days = days_param(req, 30)?;
    let log = profile::privacy_log(&ctx.profile, since_days(days));
    let policy = profile::privacy_file(&ctx.profile);
    Ok(json!({
        "days": days,
        "log_present": log.present,
        "policy_present": policy.present,
        "sentences_blocked": log.sentences_blocked(),
        "app_blocks": log.app_blocks(),
        "by_reason": log.by_reason,
        "apps_kept_off": policy.deny_exe.len(),
        "deny_exe": policy.deny_exe,
        "deny_title": policy.deny_title,
        "store_context": policy.store_context,
    }))
}

// ---- settings and word lists ----------------------------------------------------------

#[derive(Default)]
struct Patch {
    enabled: Option<bool>,
    pause_minutes: Option<u32>,
    level: Option<&'static str>,
    model: Option<String>,
    min_confidence: Option<f32>,
    autostart: Option<bool>,
}

fn canonical_level(value: &Value) -> Result<&'static str, ApiError> {
    match value.as_str().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("off") => Ok("off"),
        Some("s0") => Ok("S0"),
        Some("s1") => Ok("S1"),
        Some("s2") => Ok("S2"),
        _ => Err(ApiError::bad("auto_fix_level must be off, S0, S1 or S2")),
    }
}

/// `POST /api/settings` with any of `enabled`, `pause_minutes`, `auto_fix_level`, `model`,
/// `min_confidence`, `autostart`. Everything is validated before anything is sent; the
/// reply is the settings as the page should now show them. `{"enabled": true}` also ends a
/// pause, as in the orchestrator.
fn post_settings(ctx: &Ctx, req: &Req) -> Res {
    let body = json_body(req)?;
    let known_models: Vec<String> =
        lock(&ctx.state.settings).models.iter().map(|m| m.id.clone()).collect();
    let mut cmds = Vec::new();
    let mut patch = Patch::default();
    for (key, value) in &body {
        match key.as_str() {
            "enabled" => {
                let on = value
                    .as_bool()
                    .ok_or_else(|| ApiError::bad("enabled must be true or false"))?;
                cmds.push(UiCommand::SetEnabled(on));
                patch.enabled = Some(on);
            }
            "pause_minutes" => {
                let m = value
                    .as_u64()
                    .filter(|m| *m <= MAX_PAUSE_MINUTES)
                    .ok_or_else(|| ApiError::bad("pause_minutes must be a whole number from 0 to 1440"))?
                    as u32;
                cmds.push(UiCommand::PauseMinutes(m));
                patch.pause_minutes = Some(m);
            }
            "auto_fix_level" => {
                let level = canonical_level(value)?;
                cmds.push(UiCommand::SetAutoFixLevel(level.into()));
                patch.level = Some(level);
            }
            "model" => {
                let id = value.as_str().map(str::trim).unwrap_or("");
                let plausible = !id.is_empty()
                    && id.len() <= 100
                    && id.chars().all(|c| c.is_ascii_alphanumeric() || "._:/+-".contains(c));
                let listed = known_models.is_empty() || known_models.iter().any(|m| m == id);
                if !plausible || !listed {
                    return Err(ApiError::bad("unknown model"));
                }
                cmds.push(UiCommand::SetModel(id.into()));
                patch.model = Some(id.into());
            }
            "min_confidence" => {
                let c = value
                    .as_f64()
                    .filter(|c| (0.0..=1.0).contains(c))
                    .ok_or_else(|| ApiError::bad("min_confidence must be a number from 0 to 1"))?
                    as f32;
                cmds.push(UiCommand::SetMinConfidence(c));
                patch.min_confidence = Some(c);
            }
            "autostart" => {
                let on = value
                    .as_bool()
                    .ok_or_else(|| ApiError::bad("autostart must be true or false"))?;
                cmds.push(UiCommand::SetAutostart(on));
                patch.autostart = Some(on);
            }
            other => return Err(ApiError::bad(format!("unknown setting {other:?}"))),
        }
    }
    send(ctx, cmds)?;

    let mut s = lock(&ctx.state.settings);
    if let Some(on) = patch.enabled {
        s.enabled = on;
        s.paused_until_ms = None;
    }
    if let Some(m) = patch.pause_minutes {
        s.paused_until_ms = (m > 0).then(|| now_ms() + i64::from(m) * 60_000);
    }
    if let Some(level) = patch.level {
        s.auto_fix_level = level.into();
    }
    if let Some(model) = patch.model {
        s.model = model;
    }
    if let Some(c) = patch.min_confidence {
        s.min_confidence = c;
    }
    if let Some(on) = patch.autostart {
        s.autostart = on;
    }
    Ok(settings_json(&s))
}

/// A dictionary word or autocorrect typo: one token, no control characters.
fn clean_word(raw: Option<&str>, what: &str) -> Result<String, ApiError> {
    let w = raw.map(str::trim).unwrap_or("");
    if w.is_empty() {
        return Err(ApiError::bad(format!("{what} is required")));
    }
    if w.chars().count() > MAX_WORD_CHARS {
        return Err(ApiError::bad(format!("{what} is longer than {MAX_WORD_CHARS} characters")));
    }
    if w.chars().any(|c| c.is_whitespace() || c.is_control()) || w.contains("->") {
        return Err(ApiError::bad(format!("{what} must be a single word")));
    }
    Ok(w.to_string())
}

/// `POST /api/words` with `{"add": "word"}` or `{"remove": "word"}` (also
/// `{"action": "add"|"remove", "word": "..."}` and `{"add": true, "word": "..."}`);
/// `DELETE /api/words` removes (`default_add = Some(false)`).
fn words(ctx: &Ctx, req: &Req, default_add: Option<bool>) -> Res {
    let body = json_body(req)?;
    let word_field = body.get("word").and_then(Value::as_str);
    let mut op: Option<(bool, Option<&str>)> = default_add.map(|add| (add, word_field));
    for (key, add) in [("add", true), ("remove", false)] {
        match body.get(key) {
            Some(Value::String(w)) => op = Some((add, Some(w.as_str()))),
            Some(Value::Bool(true)) => op = Some((add, word_field)),
            _ => {}
        }
    }
    if op.is_none() {
        op = match body.get("action").or_else(|| body.get("op")).and_then(Value::as_str) {
            Some("add") => Some((true, word_field)),
            Some("remove") => Some((false, word_field)),
            _ => None,
        };
    }
    let Some((add, word)) = op else {
        return Err(ApiError::bad("expected {\"add\": word} or {\"remove\": word}"));
    };
    let word = clean_word(word.or_else(|| req.q("word")), "word")?;
    let cmd = if add {
        UiCommand::AddDictionaryWord(word.clone())
    } else {
        UiCommand::RemoveDictionaryWord(word.clone())
    };
    send(ctx, vec![cmd])?;
    Ok(json!({ "ok": true, "word": word, "added": add }))
}

/// `POST /api/autocorrect` with `{"typo": "...", "fix": "..."}`.
fn autocorrect_set(ctx: &Ctx, req: &Req) -> Res {
    let body = json_body(req)?;
    let typo = clean_word(body.get("typo").and_then(Value::as_str), "typo")?;
    let fix = body.get("fix").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if fix.is_empty() {
        return Err(ApiError::bad("fix is required"));
    }
    if fix.chars().count() > MAX_FIX_CHARS || fix.chars().any(char::is_control) {
        return Err(ApiError::bad("fix must be one line of at most 128 characters"));
    }
    if fix.to_lowercase() == typo.to_lowercase() {
        return Err(ApiError::bad("fix must differ from the typo"));
    }
    send(ctx, vec![UiCommand::SetAutocorrect(typo.clone(), fix.to_string())])?;
    Ok(json!({ "ok": true, "typo": typo, "fix": fix }))
}

/// `DELETE /api/autocorrect` with `{"typo": "..."}` or `?typo=...`.
fn autocorrect_remove(ctx: &Ctx, req: &Req) -> Res {
    let body = json_body(req)?;
    let raw = body.get("typo").and_then(Value::as_str).or_else(|| req.q("typo"));
    let typo = clean_word(raw, "typo")?;
    send(ctx, vec![UiCommand::RemoveAutocorrect(typo.clone())])?;
    Ok(json!({ "ok": true, "typo": typo }))
}
