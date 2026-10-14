//! Profile event store (M3): the `events.db` behind the learning loop.
//!
//! Every decision the app makes (an auto-fix, a suggested fix, a shown
//! completion) is one row in `events`; its `outcome` is filled in later when
//! the user keeps, reverts, accepts or overwrites it. Metrics, per-rule
//! statistics, the "recent accepted completions" prompt context and the
//! threshold tuner are all derived from that table.
//!
//! The orchestrator is single-threaded and calls this synchronously, so every
//! method is one prepared (cached) statement or one short transaction on a WAL
//! database with `synchronous=NORMAL` (no fsync per commit).
//!
//! Conventions:
//! * Timestamps are unix milliseconds.
//! * A *completion* is an event whose source is [`Source::LlmComplete`]; every
//!   other source produces a *fix* (auto-applied or suggested).
//! * `rule_overrides` rows with `max_severity = 'suggest'` are demotions. A row
//!   with `max_severity = 'auto'` is the marker left by
//!   [`Store::clear_demotion`]: it records when the demotion was cleared so
//!   [`Store::tune`] only weighs auto-fixes made after that point instead of
//!   immediately demoting the rule again from the same old reverts.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, TransactionBehavior};

const SCHEMA_VERSION: i64 = 1;
const MAX_CONTEXT_CHARS: usize = 200;

/// Minimum auto-fixes before a rule can be demoted.
const DEMOTE_MIN_AUTO: u64 = 20;
/// Minimum suggestions before a promotion can be offered.
const PROMOTE_MIN_SUGGESTED: u64 = 50;

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS events(
    id             INTEGER PRIMARY KEY,
    ts             INTEGER NOT NULL,
    app_exe        TEXT    NOT NULL,
    source         TEXT    NOT NULL,
    rule           TEXT    NOT NULL,
    severity       TEXT    NOT NULL,
    confidence     REAL    NOT NULL,
    context_before TEXT,
    original       TEXT    NOT NULL,
    replacement    TEXT    NOT NULL,
    action         TEXT    NOT NULL,
    outcome        TEXT,
    outcome_ts     INTEGER,
    latency_ms     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_rule    ON events(rule);
CREATE INDEX IF NOT EXISTS idx_events_ts      ON events(ts);
CREATE INDEX IF NOT EXISTS idx_events_outcome ON events(outcome);
CREATE TABLE IF NOT EXISTS rule_overrides(
    rule         TEXT PRIMARY KEY,
    max_severity TEXT    NOT NULL,
    since        INTEGER NOT NULL
);
";

/// Which engine produced an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Harper,
    Autocorrect,
    Dict,
    LlmFix,
    LlmComplete,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Harper => "harper",
            Source::Autocorrect => "autocorrect",
            Source::Dict => "dict",
            Source::LlmFix => "llm_fix",
            Source::LlmComplete => "llm_complete",
        }
    }
}

/// Whether the change was applied without asking or only offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Auto,
    Suggested,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Auto => "auto",
            Action::Suggested => "suggested",
        }
    }
}

/// What eventually happened to an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Kept,
    Reverted,
    Accepted,
    Ignored,
    Overwritten,
    Timeout,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Kept => "kept",
            Outcome::Reverted => "reverted",
            Outcome::Accepted => "accepted",
            Outcome::Ignored => "ignored",
            Outcome::Overwritten => "overwritten",
            Outcome::Timeout => "timeout",
        }
    }
}

/// An event about to be logged.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub app_exe: String,
    pub source: Source,
    /// e.g. "spell", "autocorrect", "sentence_capitalization", "complete"
    pub rule: String,
    /// "S0".."S4"
    pub severity: String,
    pub confidence: f32,
    /// Up to 200 chars left of the span (longer input keeps the last 200);
    /// the caller decides about privacy.
    pub context_before: Option<String>,
    /// For completions: ""; for fixes: the typed word.
    pub original: String,
    /// The fix, or the suggested completion text.
    pub replacement: String,
    pub action: Action,
    pub latency_ms: u32,
}

/// Aggregate numbers shown in the tray.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    /// All events in the window (fixes and completions).
    pub events: u64,
    /// Characters of accepted completions plus characters of kept fixes.
    pub chars_saved: u64,
    /// Fixes applied automatically.
    pub fixes_auto: u64,
    /// Automatic fixes the user reverted.
    pub fixes_reverted: u64,
    /// Suggested fixes (not completions) the user accepted.
    pub suggestions_accepted: u64,
    pub completions_shown: u64,
    pub completions_accepted: u64,
    /// Nearest-rank percentiles of `latency_ms` over all events in the window.
    pub p50_latency_ms: u32,
    pub p95_latency_ms: u32,
}

/// Per-rule counters (all time).
#[derive(Debug, Clone)]
pub struct RuleStat {
    pub rule: String,
    pub auto_total: u64,
    pub auto_reverted: u64,
    pub suggested_total: u64,
    pub suggested_accepted: u64,
}

/// A recommendation from [`Store::tune`].
#[derive(Debug, Clone, PartialEq)]
pub enum Tuning {
    /// The rule was reverted too often and is now capped at "suggest".
    Demote { rule: String, reason: String },
    /// The rule's suggestions are almost always accepted; offer to auto-apply.
    OfferPromotion { rule: String, reason: String },
}

/// Handle to a profile's `events.db`.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating the directory and file if needed) and migrate.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create {}", dir.display()))?;
            }
        }
        let mut conn = Connection::open(path)
            .with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        // journal_mode returns a row, so it cannot go through execute().
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        migrate(&mut conn)?;
        Ok(Store { conn })
    }

    /// Record an event; returns its id. `ts` is the current unix time in ms.
    pub fn log(&self, ev: &NewEvent) -> Result<i64> {
        let confidence = if ev.confidence.is_finite() {
            f64::from(ev.confidence).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let ctx = ev
            .context_before
            .as_deref()
            .map(|c| tail_chars(c, MAX_CONTEXT_CHARS));
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO events(ts, app_exe, source, rule, severity, confidence, \
             context_before, original, replacement, action, latency_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        stmt.execute(params![
            now_ms(),
            ev.app_exe,
            ev.source.as_str(),
            ev.rule,
            ev.severity,
            confidence,
            ctx,
            ev.original,
            ev.replacement,
            ev.action.as_str(),
            i64::from(ev.latency_ms),
        ])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Set the outcome of an event (and `outcome_ts` to now).
    ///
    /// The first outcome wins, except that `Reverted` and `Overwritten`
    /// replace a previous `Kept`. Repeating a call is a no-op, and so is an
    /// unknown id.
    pub fn set_outcome(&self, id: i64, outcome: Outcome) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "UPDATE events SET outcome = ?1, outcome_ts = ?2 \
             WHERE id = ?3 AND (outcome IS NULL OR \
               (outcome = 'kept' AND ?1 IN ('reverted', 'overwritten')))",
        )?;
        stmt.execute(params![outcome.as_str(), now_ms(), id])?;
        Ok(())
    }

    /// Aggregate metrics over events with `ts >= since_unix_ms` (all if `None`).
    ///
    /// `chars_saved` = characters of accepted completions + characters of the
    /// typed word for every kept fix.
    pub fn metrics(&self, since_unix_ms: Option<i64>) -> Result<Metrics> {
        let since = since_unix_ms.unwrap_or(i64::MIN);
        let tx = self.conn.unchecked_transaction()?;
        let mut m = tx.query_row(
            "SELECT COUNT(*), \
               COALESCE(SUM(CASE \
                 WHEN source = 'llm_complete' AND outcome = 'accepted' THEN length(replacement) \
                 WHEN source <> 'llm_complete' AND outcome = 'kept' THEN length(original) \
                 ELSE 0 END), 0), \
               COUNT(CASE WHEN source <> 'llm_complete' AND action = 'auto' THEN 1 END), \
               COUNT(CASE WHEN source <> 'llm_complete' AND action = 'auto' \
                          AND outcome = 'reverted' THEN 1 END), \
               COUNT(CASE WHEN source <> 'llm_complete' AND action = 'suggested' \
                          AND outcome = 'accepted' THEN 1 END), \
               COUNT(CASE WHEN source = 'llm_complete' THEN 1 END), \
               COUNT(CASE WHEN source = 'llm_complete' AND outcome = 'accepted' THEN 1 END) \
             FROM events WHERE ts >= ?1",
            [since],
            |r| {
                Ok(Metrics {
                    events: cnt(r.get(0)?),
                    chars_saved: cnt(r.get(1)?),
                    fixes_auto: cnt(r.get(2)?),
                    fixes_reverted: cnt(r.get(3)?),
                    suggestions_accepted: cnt(r.get(4)?),
                    completions_shown: cnt(r.get(5)?),
                    completions_accepted: cnt(r.get(6)?),
                    p50_latency_ms: 0,
                    p95_latency_ms: 0,
                })
            },
        )?;
        if m.events > 0 {
            m.p50_latency_ms = percentile(&tx, since, m.events, 50)?;
            m.p95_latency_ms = percentile(&tx, since, m.events, 95)?;
        }
        tx.commit()?;
        Ok(m)
    }

    /// Per-rule counters over all time, for rules with at least `min_events`
    /// events, busiest first.
    pub fn rule_stats(&self, min_events: u64) -> Result<Vec<RuleStat>> {
        let min = i64::try_from(min_events).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare_cached(
            "SELECT rule, \
               COUNT(CASE WHEN action = 'auto' THEN 1 END), \
               COUNT(CASE WHEN action = 'auto' AND outcome = 'reverted' THEN 1 END), \
               COUNT(CASE WHEN action = 'suggested' THEN 1 END), \
               COUNT(CASE WHEN action = 'suggested' AND outcome = 'accepted' THEN 1 END) \
             FROM events GROUP BY rule HAVING COUNT(*) >= ?1 \
             ORDER BY COUNT(*) DESC, rule",
        )?;
        let rows = stmt.query_map([min], |r| {
            Ok(RuleStat {
                rule: r.get(0)?,
                auto_total: cnt(r.get(1)?),
                auto_reverted: cnt(r.get(2)?),
                suggested_total: cnt(r.get(3)?),
                suggested_accepted: cnt(r.get(4)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replacement text of completions the user accepted, newest first,
    /// optionally restricted to one app (exe names compare case-insensitively).
    pub fn recent_accepted(&self, n: usize, app_exe: Option<&str>) -> Result<Vec<String>> {
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare_cached(
            "SELECT replacement FROM events \
             WHERE source = 'llm_complete' AND outcome = 'accepted' \
               AND (?2 IS NULL OR app_exe = ?2 COLLATE NOCASE) \
             ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit, app_exe], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Evaluate every rule and return recommendations.
    ///
    /// * `auto_total >= 20 && auto_reverted * 4 > auto_total` -> `Demote`,
    ///   persisted in `rule_overrides` (so a rule is reported once).
    /// * `suggested_total >= 50 && suggested_accepted * 10 >= suggested_total * 9`
    ///   -> `OfferPromotion` (not persisted, so it is reported on every call
    ///   until acted upon).
    pub fn tune(&self) -> Result<Vec<Tuning>> {
        let tx = self.conn.unchecked_transaction()?;
        // After clear_demotion the rule's history restarts at the clear time.
        let rows: Vec<(String, u64, u64, u64, u64, bool)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT e.rule, \
                   COUNT(CASE WHEN e.action = 'auto' THEN 1 END), \
                   COUNT(CASE WHEN e.action = 'auto' AND e.outcome = 'reverted' THEN 1 END), \
                   COUNT(CASE WHEN e.action = 'suggested' THEN 1 END), \
                   COUNT(CASE WHEN e.action = 'suggested' AND e.outcome = 'accepted' THEN 1 END), \
                   COALESCE(MAX(o.max_severity = 'suggest'), 0) \
                 FROM events e LEFT JOIN rule_overrides o ON o.rule = e.rule \
                 WHERE o.rule IS NULL OR o.max_severity <> 'auto' OR e.ts > o.since \
                 GROUP BY e.rule ORDER BY e.rule",
            )?;
            let it = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    cnt(r.get(1)?),
                    cnt(r.get(2)?),
                    cnt(r.get(3)?),
                    cnt(r.get(4)?),
                    r.get::<_, i64>(5)? != 0,
                ))
            })?;
            it.collect::<rusqlite::Result<_>>()?
        };

        let now = now_ms();
        let mut out = Vec::new();
        for (rule, auto_total, auto_reverted, sug_total, sug_accepted, demoted) in rows {
            if !demoted && auto_total >= DEMOTE_MIN_AUTO && auto_reverted * 4 > auto_total {
                tx.prepare_cached(
                    "INSERT INTO rule_overrides(rule, max_severity, since) \
                     VALUES (?1, 'suggest', ?2) \
                     ON CONFLICT(rule) DO UPDATE SET \
                       max_severity = 'suggest', since = excluded.since",
                )?
                .execute(params![rule, now])?;
                out.push(Tuning::Demote {
                    reason: format!(
                        "{auto_reverted} of {auto_total} auto-fixes reverted ({}%)",
                        pct(auto_reverted, auto_total)
                    ),
                    rule: rule.clone(),
                });
            }
            if sug_total >= PROMOTE_MIN_SUGGESTED && sug_accepted * 10 >= sug_total * 9 {
                out.push(Tuning::OfferPromotion {
                    reason: format!(
                        "{sug_accepted} of {sug_total} suggestions accepted ({}%)",
                        pct(sug_accepted, sug_total)
                    ),
                    rule,
                });
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// Is the rule currently capped at "suggest"?
    pub fn is_demoted(&self, rule: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM rule_overrides \
             WHERE rule = ?1 AND max_severity = 'suggest')",
        )?;
        Ok(stmt.query_row([rule], |r| r.get::<_, i64>(0))? != 0)
    }

    /// Lift a demotion. The tuner then only counts the rule's events logged
    /// after this moment. No-op if the rule is not demoted.
    pub fn clear_demotion(&self, rule: &str) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "UPDATE rule_overrides SET max_severity = 'auto', since = ?2 \
             WHERE rule = ?1 AND max_severity = 'suggest'",
        )?;
        stmt.execute(params![rule, now_ms()])?;
        Ok(())
    }

    /// Mark events that still have no outcome and were logged at least
    /// `older_than_ms` ago (an age, not a timestamp) as `Timeout`. Returns
    /// how many were updated.
    pub fn expire_pending(&self, older_than_ms: i64) -> Result<u64> {
        let now = now_ms();
        let cutoff = now.saturating_sub(older_than_ms.max(0));
        let mut stmt = self.conn.prepare_cached(
            "UPDATE events SET outcome = 'timeout', outcome_ts = ?1 \
             WHERE outcome IS NULL AND ts <= ?2",
        )?;
        let n = stmt.execute(params![now, cutoff])?;
        Ok(n as u64)
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    // IMMEDIATE so two processes opening a fresh profile do not both migrate.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS schema_version(version INTEGER NOT NULL);")?;
    let current: i64 = tx.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    if current > SCHEMA_VERSION {
        bail!(
            "events.db has schema version {current}, this build only knows up to {SCHEMA_VERSION}"
        );
    }
    if current < 1 {
        tx.execute_batch(SCHEMA_V1)?;
    }
    if current < SCHEMA_VERSION {
        tx.execute("DELETE FROM schema_version", [])?;
        tx.execute("INSERT INTO schema_version(version) VALUES (?1)", [SCHEMA_VERSION])?;
    }
    tx.commit()?;
    Ok(())
}

/// Nearest-rank percentile of `latency_ms` among the `n` events since `since`.
fn percentile(conn: &Connection, since: i64, n: u64, p: u64) -> Result<u32> {
    let rank = (n * p).div_ceil(100).max(1); // 1-based
    let offset = i64::try_from(rank - 1).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare_cached(
        "SELECT latency_ms FROM events WHERE ts >= ?1 ORDER BY latency_ms LIMIT 1 OFFSET ?2",
    )?;
    let v = match stmt.query_row(params![since, offset], |r| r.get::<_, i64>(0)) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => 0,
        Err(e) => return Err(e.into()),
    };
    Ok(u32::try_from(v.max(0)).unwrap_or(u32::MAX))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cnt(v: i64) -> u64 {
    v.max(0) as u64
}

fn pct(part: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        (part * 100 + total / 2) / total
    }
}

/// The last `n` chars of `s`.
fn tail_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let idx = s
        .char_indices()
        .nth(count - n)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    &s[idx..]
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
                "cc-store-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        /// A db path two directories deep, to exercise directory creation.
        fn db(&self) -> PathBuf {
            self.0.join("profiles").join("default").join("events.db")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fix(rule: &str, action: Action, original: &str, replacement: &str) -> NewEvent {
        NewEvent {
            app_exe: "notepad.exe".into(),
            source: Source::Harper,
            rule: rule.into(),
            severity: "S1".into(),
            confidence: 0.9,
            context_before: None,
            original: original.into(),
            replacement: replacement.into(),
            action,
            latency_ms: 5,
        }
    }

    fn completion(app: &str, text: &str) -> NewEvent {
        NewEvent {
            app_exe: app.into(),
            source: Source::LlmComplete,
            rule: "complete".into(),
            severity: "S4".into(),
            confidence: 0.5,
            context_before: Some("so far ".into()),
            original: String::new(),
            replacement: text.into(),
            action: Action::Suggested,
            latency_ms: 50,
        }
    }

    fn outcome_of(s: &Store, id: i64) -> (Option<String>, Option<i64>) {
        s.conn
            .query_row(
                "SELECT outcome, outcome_ts FROM events WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn set_ts(s: &Store, id: i64, ts: i64) {
        s.conn
            .execute("UPDATE events SET ts = ?1 WHERE id = ?2", params![ts, id])
            .unwrap();
    }

    fn set_latency(s: &Store, id: i64, ms: i64) {
        s.conn
            .execute("UPDATE events SET latency_ms = ?1 WHERE id = ?2", params![ms, id])
            .unwrap();
    }

    /// Log `n` events of `rule`, the first `reverted` of them with outcome Reverted.
    fn log_auto(s: &Store, rule: &str, n: usize, reverted: usize) {
        for i in 0..n {
            let id = s.log(&fix(rule, Action::Auto, "teh", "the")).unwrap();
            let o = if i < reverted { Outcome::Reverted } else { Outcome::Kept };
            s.set_outcome(id, o).unwrap();
        }
    }

    fn log_suggested(s: &Store, rule: &str, n: usize, accepted: usize) {
        for i in 0..n {
            let id = s.log(&fix(rule, Action::Suggested, "wierd", "weird")).unwrap();
            let o = if i < accepted { Outcome::Accepted } else { Outcome::Ignored };
            s.set_outcome(id, o).unwrap();
        }
    }

    #[test]
    fn open_twice_is_idempotent() {
        let dir = TempDir::new();
        let path = dir.db();
        let id = {
            let s = Store::open(&path).unwrap();
            s.log(&fix("spell", Action::Auto, "teh", "the")).unwrap()
        };
        let s = Store::open(&path).unwrap();
        let s2 = Store::open(&path).unwrap(); // concurrent second handle is fine too
        let n: i64 = s.conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let rows: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!((rows, v), (1, SCHEMA_VERSION));
        let mode: String = s.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        s2.set_outcome(id, Outcome::Kept).unwrap();
        assert_eq!(outcome_of(&s, id).0.as_deref(), Some("kept"));
    }

    #[test]
    fn rejects_newer_schema() {
        let dir = TempDir::new();
        let path = dir.db();
        {
            let s = Store::open(&path).unwrap();
            s.conn.execute("UPDATE schema_version SET version = 99", []).unwrap();
        }
        assert!(Store::open(&path).is_err());
    }

    #[test]
    fn log_normalises_input() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        let mut ev = fix("spell", Action::Auto, "teh", "the");
        ev.context_before = Some(format!("{}{}", "x".repeat(300), "é".repeat(200)));
        ev.confidence = f32::NAN;
        let id = s.log(&ev).unwrap();
        let (ctx, conf, ts): (String, f64, i64) = s
            .conn
            .query_row(
                "SELECT context_before, confidence, ts FROM events WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(ctx.chars().count(), 200);
        assert!(ctx.chars().all(|c| c == 'é'));
        assert_eq!(conf, 0.0);
        assert!((now_ms() - ts).abs() < 5_000);
    }

    #[test]
    fn set_outcome_rules() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        let mk = || s.log(&fix("spell", Action::Auto, "teh", "the")).unwrap();

        // No outcome yet.
        let a = mk();
        assert_eq!(outcome_of(&s, a), (None, None));

        // First outcome wins.
        s.set_outcome(a, Outcome::Accepted).unwrap();
        let (o, ts) = outcome_of(&s, a);
        assert_eq!(o.as_deref(), Some("accepted"));
        assert!(ts.is_some());
        s.set_outcome(a, Outcome::Ignored).unwrap();
        s.set_outcome(a, Outcome::Reverted).unwrap(); // only Kept is overridable
        assert_eq!(outcome_of(&s, a).0.as_deref(), Some("accepted"));

        // Kept is overridden by Reverted / Overwritten.
        let b = mk();
        s.set_outcome(b, Outcome::Kept).unwrap();
        s.set_outcome(b, Outcome::Reverted).unwrap();
        assert_eq!(outcome_of(&s, b).0.as_deref(), Some("reverted"));
        s.set_outcome(b, Outcome::Kept).unwrap(); // and does not come back
        s.set_outcome(b, Outcome::Overwritten).unwrap();
        assert_eq!(outcome_of(&s, b).0.as_deref(), Some("reverted"));

        let c = mk();
        s.set_outcome(c, Outcome::Kept).unwrap();
        s.set_outcome(c, Outcome::Overwritten).unwrap();
        assert_eq!(outcome_of(&s, c).0.as_deref(), Some("overwritten"));

        // Kept is not overridden by non-negative outcomes; repeats are no-ops.
        let d = mk();
        s.set_outcome(d, Outcome::Kept).unwrap();
        let first = outcome_of(&s, d);
        std::thread::sleep(std::time::Duration::from_millis(3));
        s.set_outcome(d, Outcome::Kept).unwrap();
        s.set_outcome(d, Outcome::Accepted).unwrap();
        s.set_outcome(d, Outcome::Timeout).unwrap();
        assert_eq!(outcome_of(&s, d), first);

        // Unknown id is not an error.
        s.set_outcome(987_654, Outcome::Kept).unwrap();
    }

    #[test]
    fn metrics_fixture() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();

        let mut ids = Vec::new();
        // 0,1: accepted completions (11 and 5 chars; "héllo" is 6 bytes)
        for text in ["hello world", "héllo"] {
            let id = s.log(&completion("notepad.exe", text)).unwrap();
            s.set_outcome(id, Outcome::Accepted).unwrap();
            ids.push(id);
        }
        // 2: shown but ignored completion
        let id = s.log(&completion("notepad.exe", "ignored text")).unwrap();
        s.set_outcome(id, Outcome::Ignored).unwrap();
        ids.push(id);
        // 3: kept auto-fix "teh" (3 chars saved)
        let id = s.log(&fix("spell", Action::Auto, "teh", "the")).unwrap();
        s.set_outcome(id, Outcome::Kept).unwrap();
        ids.push(id);
        // 4: reverted auto-fix
        let id = s.log(&fix("spell", Action::Auto, "recieve", "receive")).unwrap();
        s.set_outcome(id, Outcome::Reverted).unwrap();
        ids.push(id);
        // 5: accepted suggestion (no chars_saved: only kept fixes count)
        let id = s.log(&fix("grammar", Action::Suggested, "wierd", "weird")).unwrap();
        s.set_outcome(id, Outcome::Accepted).unwrap();
        ids.push(id);
        // 6: overwritten auto-fix
        let id = s.log(&fix("spell", Action::Auto, "xx", "yy")).unwrap();
        s.set_outcome(id, Outcome::Overwritten).unwrap();
        ids.push(id);

        for (id, ms) in ids.iter().zip([5, 1, 9, 3, 7, 2, 100]) {
            set_latency(&s, *id, ms);
        }

        let m = s.metrics(None).unwrap();
        assert_eq!(m.events, 7);
        assert_eq!(m.chars_saved, 11 + 5 + 3);
        assert_eq!(m.fixes_auto, 3);
        assert_eq!(m.fixes_reverted, 1);
        assert_eq!(m.suggestions_accepted, 1);
        assert_eq!(m.completions_shown, 3);
        assert_eq!(m.completions_accepted, 2);
        // sorted: 1 2 3 5 7 9 100 -> nearest rank p50 = 4th = 5, p95 = 7th = 100
        assert_eq!(m.p50_latency_ms, 5);
        assert_eq!(m.p95_latency_ms, 100);

        // `since` window: push the reverted and overwritten fixes into the past.
        set_ts(&s, ids[4], 1_000);
        set_ts(&s, ids[6], 1_000);
        let m = s.metrics(Some(10_000)).unwrap();
        assert_eq!(m.events, 5);
        assert_eq!(m.fixes_auto, 1);
        assert_eq!(m.fixes_reverted, 0);
        assert_eq!(m.chars_saved, 19);
        // remaining latencies sorted: 1 2 3 5 9 -> p50 = 3rd = 3, p95 = 5th = 9
        assert_eq!((m.p50_latency_ms, m.p95_latency_ms), (3, 9));

        // Empty window.
        let m = s.metrics(Some(now_ms() + 60_000)).unwrap();
        assert_eq!((m.events, m.chars_saved, m.p50_latency_ms, m.p95_latency_ms), (0, 0, 0, 0));
    }

    #[test]
    fn metrics_percentiles_of_ten() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        for ms in 1..=10 {
            let id = s.log(&completion("a.exe", "x")).unwrap();
            set_latency(&s, id, ms);
        }
        let m = s.metrics(None).unwrap();
        assert_eq!((m.p50_latency_ms, m.p95_latency_ms), (5, 10));
    }

    #[test]
    fn rule_stats_counts_and_filters() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        log_auto(&s, "spell", 6, 2);
        log_suggested(&s, "spell", 3, 1);
        log_suggested(&s, "grammar", 4, 4);
        log_auto(&s, "rare", 1, 1);
        let id = s.log(&completion("a.exe", "text")).unwrap();
        s.set_outcome(id, Outcome::Accepted).unwrap(); // completions show up under rule "complete"

        let all = s.rule_stats(0).unwrap();
        assert_eq!(all.len(), 4);
        // busiest first
        assert_eq!(all[0].rule, "spell");
        assert_eq!(
            (all[0].auto_total, all[0].auto_reverted, all[0].suggested_total, all[0].suggested_accepted),
            (6, 2, 3, 1)
        );
        let grammar = all.iter().find(|r| r.rule == "grammar").unwrap();
        assert_eq!(
            (grammar.auto_total, grammar.auto_reverted, grammar.suggested_total, grammar.suggested_accepted),
            (0, 0, 4, 4)
        );

        let some = s.rule_stats(4).unwrap();
        let names: Vec<_> = some.iter().map(|r| r.rule.as_str()).collect();
        assert_eq!(names, ["spell", "grammar"]);
        assert!(s.rule_stats(100).unwrap().is_empty());
    }

    #[test]
    fn recent_accepted_order_and_filter() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();

        let log_acc = |app: &str, text: &str, ts: i64, accept: bool| {
            let id = s.log(&completion(app, text)).unwrap();
            set_ts(&s, id, ts);
            if accept {
                s.set_outcome(id, Outcome::Accepted).unwrap();
            } else {
                s.set_outcome(id, Outcome::Ignored).unwrap();
            }
            id
        };
        log_acc("Notepad.exe", "oldest", 100, true);
        log_acc("chrome.exe", "chrome one", 200, true);
        log_acc("notepad.exe", "ignored", 250, false);
        log_acc("notepad.exe", "newest", 300, true);
        // Inserted last but older timestamp: must sort by ts, not insertion.
        log_acc("notepad.exe", "middle", 150, true);
        // Equal ts: later id first.
        log_acc("chrome.exe", "chrome tie a", 400, true);
        log_acc("chrome.exe", "chrome tie b", 400, true);
        // A fix with Accepted must not appear.
        let fid = s.log(&fix("spell", Action::Suggested, "teh", "the")).unwrap();
        s.set_outcome(fid, Outcome::Accepted).unwrap();
        set_ts(&s, fid, 500);

        assert_eq!(
            s.recent_accepted(10, None).unwrap(),
            ["chrome tie b", "chrome tie a", "newest", "chrome one", "middle", "oldest"]
        );
        assert_eq!(s.recent_accepted(2, None).unwrap(), ["chrome tie b", "chrome tie a"]);
        assert_eq!(
            s.recent_accepted(10, Some("notepad.exe")).unwrap(),
            ["newest", "middle", "oldest"] // "Notepad.exe" matches case-insensitively
        );
        assert_eq!(
            s.recent_accepted(10, Some("chrome.exe")).unwrap(),
            ["chrome tie b", "chrome tie a", "chrome one"]
        );
        assert!(s.recent_accepted(10, Some("nothing.exe")).unwrap().is_empty());
        assert!(s.recent_accepted(0, None).unwrap().is_empty());
    }

    #[test]
    fn tune_demotes_and_is_idempotent() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        log_auto(&s, "bad_rule", 25, 8); // 32% reverted
        log_auto(&s, "ok_rule", 25, 4); // 16% reverted
        log_auto(&s, "young_rule", 19, 19); // too few events
        log_auto(&s, "edge_rule", 20, 5); // exactly 25%: not demoted

        let t = s.tune().unwrap();
        assert_eq!(t.len(), 1, "{t:?}");
        match &t[0] {
            Tuning::Demote { rule, reason } => {
                assert_eq!(rule, "bad_rule");
                assert!(reason.contains("8 of 25"), "{reason}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(s.is_demoted("bad_rule").unwrap());
        assert!(!s.is_demoted("ok_rule").unwrap());
        assert!(!s.is_demoted("young_rule").unwrap());
        assert!(!s.is_demoted("edge_rule").unwrap());

        // Idempotent: nothing new to report, still demoted, one override row.
        assert!(s.tune().unwrap().is_empty());
        assert!(s.is_demoted("bad_rule").unwrap());
        let rows: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM rule_overrides", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        let sev: String = s
            .conn
            .query_row(
                "SELECT max_severity FROM rule_overrides WHERE rule = 'bad_rule'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sev, "suggest");

        // Persisted across reopen.
        drop(s);
        let s = Store::open(&dir.db()).unwrap();
        assert!(s.is_demoted("bad_rule").unwrap());
        assert!(s.tune().unwrap().is_empty());
    }

    #[test]
    fn tune_offers_promotion_without_persisting() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        log_suggested(&s, "good", 50, 45); // exactly 90%
        log_suggested(&s, "almost", 50, 44); // 88%
        log_suggested(&s, "few", 49, 49); // too few

        let expect = |t: Vec<Tuning>| {
            assert_eq!(t.len(), 1, "{t:?}");
            match &t[0] {
                Tuning::OfferPromotion { rule, reason } => {
                    assert_eq!(rule, "good");
                    assert!(reason.contains("45 of 50"), "{reason}");
                }
                other => panic!("unexpected {other:?}"),
            }
        };
        expect(s.tune().unwrap());
        expect(s.tune().unwrap()); // not persisted: offered again
        assert!(!s.is_demoted("good").unwrap());
        let rows: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM rule_overrides", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn clear_demotion_restarts_the_rule_history() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        s.clear_demotion("never_demoted").unwrap(); // no-op
        assert!(!s.is_demoted("never_demoted").unwrap());

        log_auto(&s, "bad_rule", 25, 8);
        assert_eq!(s.tune().unwrap().len(), 1);
        assert!(s.is_demoted("bad_rule").unwrap());

        s.clear_demotion("bad_rule").unwrap();
        assert!(!s.is_demoted("bad_rule").unwrap());
        // The old reverts must not demote it again right away.
        assert!(s.tune().unwrap().is_empty());
        assert!(!s.is_demoted("bad_rule").unwrap());

        // Fresh evidence after the clear can demote it again.
        std::thread::sleep(std::time::Duration::from_millis(5));
        log_auto(&s, "bad_rule", 20, 10);
        let t = s.tune().unwrap();
        assert_eq!(t.len(), 1, "{t:?}");
        assert!(matches!(&t[0], Tuning::Demote { rule, .. } if rule == "bad_rule"));
        assert!(s.is_demoted("bad_rule").unwrap());
    }

    #[test]
    fn expire_pending_marks_old_unresolved_events() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        let old_pending = s.log(&fix("spell", Action::Auto, "teh", "the")).unwrap();
        let old_done = s.log(&fix("spell", Action::Auto, "adn", "and")).unwrap();
        let fresh = s.log(&fix("spell", Action::Auto, "taht", "that")).unwrap();
        s.set_outcome(old_done, Outcome::Kept).unwrap();
        set_ts(&s, old_pending, now_ms() - 60_000);
        set_ts(&s, old_done, now_ms() - 60_000);

        assert_eq!(s.expire_pending(30_000).unwrap(), 1);
        let (o, ts) = outcome_of(&s, old_pending);
        assert_eq!(o.as_deref(), Some("timeout"));
        assert!(ts.is_some());
        assert_eq!(outcome_of(&s, old_done).0.as_deref(), Some("kept"));
        assert_eq!(outcome_of(&s, fresh).0, None);

        assert_eq!(s.expire_pending(30_000).unwrap(), 0); // idempotent
        // Age 0 expires everything still pending.
        assert_eq!(s.expire_pending(0).unwrap(), 1);
        assert_eq!(outcome_of(&s, fresh).0.as_deref(), Some("timeout"));
    }

    #[test]
    fn tail_chars_boundaries() {
        assert_eq!(tail_chars("abc", 5), "abc");
        assert_eq!(tail_chars("abc", 3), "abc");
        assert_eq!(tail_chars("abc", 2), "bc");
        assert_eq!(tail_chars("aéb", 2), "éb");
        assert_eq!(tail_chars("abc", 0), "");
    }
}
