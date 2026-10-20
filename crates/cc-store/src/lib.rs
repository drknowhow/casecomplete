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
/// An auto-fix backspaced over this soon after it went in counts as undone, like a revert.
const QUICK_OVERWRITE_MS: i64 = 3000;
/// How many of an app's latest fix checks [`Store::landing`] counts.
const LANDING_WINDOW: u32 = 50;
/// Checks an app needs before [`Landing::too_low`] can say it does not take fixes cleanly.
const LANDING_MIN_CHECKS: u64 = 20;
/// Checks older than this are forgotten, so an app gets auto-fix back after a quiet week.
const LANDING_MAX_AGE_MS: i64 = 7 * 86_400_000;

/// Tables added after version 1. Each is created if missing on every open instead of
/// bumping `SCHEMA_VERSION`, so an older build can still open the file and ignores them.
const SCHEMA_ADDED: &str = "
CREATE TABLE IF NOT EXISTS fix_checks(
    event_id INTEGER NOT NULL,
    ts       INTEGER NOT NULL,
    app_exe  TEXT    NOT NULL,
    landed   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_fix_checks_app ON fix_checks(app_exe COLLATE NOCASE, ts);
";

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
    /// Undone: reverted, or backspaced over within 3 s.
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

const DAY_MS: i64 = 86_400_000;

/// Width of one [`Store::latency_histogram`] bucket.
pub const LATENCY_BUCKET_MS: u32 = 20;
/// Number of [`Store::latency_histogram`] buckets: 15 of 20 ms (0..300 ms) plus one open
/// bucket for everything from 300 ms up.
pub const LATENCY_BUCKETS: usize = 16;
/// Number of confidence buckets (deciles) in [`Store::by_confidence`].
pub const CONFIDENCE_BUCKETS: usize = 10;

/// One calendar day of [`Store::daily_series`].
///
/// "Shown"/"accepted" count completions; "fixes" count every other source. A fix is
/// *kept* when its outcome is `kept` (auto-applied, stayed) or `accepted` (a suggested
/// fix the user took).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayStat {
    /// `YYYY-MM-DD` in the requested UTC offset.
    pub date: String,
    pub shown: u64,
    pub accepted: u64,
    pub fixes_kept: u64,
    pub fixes_reverted: u64,
    /// Same definition as [`Metrics::chars_saved`].
    pub chars_saved: u64,
}

/// Completion latency distribution from [`Store::latency_histogram`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyHistogram {
    /// [`LATENCY_BUCKETS`] counts; bucket `i < 15` covers `[i*20, i*20+20)` ms, the last one
    /// is `300 ms and up`.
    pub counts: Vec<u64>,
    pub total: u64,
    /// Nearest-rank percentiles over the same completion events (0 when empty).
    pub p50_ms: u32,
    pub p95_ms: u32,
}

/// Per-app counters from [`Store::by_app`] (exe names compare case-insensitively).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppStat {
    pub app_exe: String,
    pub fixes_kept: u64,
    pub fixes_reverted: u64,
    /// Completions shown.
    pub shown: u64,
    /// Completions accepted.
    pub accepted: u64,
}

/// How an app's latest auto-fixes landed when read back from the field ([`Store::landing`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Landing {
    pub checked: u64,
    /// Read back different from what was typed.
    pub failed: u64,
}

impl Landing {
    /// More than one in fifty fixes went wrong, over enough fixes to tell.
    pub fn too_low(&self) -> bool {
        self.checked >= LANDING_MIN_CHECKS && self.failed * 50 > self.checked
    }
}

/// One confidence decile of [`Store::by_confidence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfidenceBucket {
    /// 0..=9; bucket `b` covers confidence `[b/10, (b+1)/10)` (the last one includes 1.0).
    pub bucket: u32,
    pub shown: u64,
    pub accepted: u64,
}

/// Which events [`Store::recent_events`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    All,
    /// Every source except completions.
    Fix,
    /// Completions only.
    Complete,
}

/// One row of the event log, as read by [`Store::recent_events`].
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub id: i64,
    pub ts: i64,
    pub app_exe: String,
    pub source: String,
    pub rule: String,
    pub severity: String,
    pub confidence: f64,
    pub context_before: Option<String>,
    pub original: String,
    pub replacement: String,
    pub action: String,
    pub outcome: Option<String>,
    pub latency_ms: u32,
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
               COUNT(CASE WHEN action = 'auto' AND (outcome = 'reverted' \
                 OR (outcome = 'overwritten' AND outcome_ts - ts <= ?2)) THEN 1 END), \
               COUNT(CASE WHEN action = 'suggested' THEN 1 END), \
               COUNT(CASE WHEN action = 'suggested' AND outcome = 'accepted' THEN 1 END) \
             FROM events GROUP BY rule HAVING COUNT(*) >= ?1 \
             ORDER BY COUNT(*) DESC, rule",
        )?;
        let rows = stmt.query_map(params![min, QUICK_OVERWRITE_MS], |r| {
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
                   COUNT(CASE WHEN e.action = 'auto' AND (e.outcome = 'reverted' \
                     OR (e.outcome = 'overwritten' AND e.outcome_ts - e.ts <= ?1)) THEN 1 END), \
                   COUNT(CASE WHEN e.action = 'suggested' THEN 1 END), \
                   COUNT(CASE WHEN e.action = 'suggested' AND e.outcome = 'accepted' THEN 1 END), \
                   COALESCE(MAX(o.max_severity = 'suggest'), 0) \
                 FROM events e LEFT JOIN rule_overrides o ON o.rule = e.rule \
                 WHERE o.rule IS NULL OR o.max_severity <> 'auto' OR e.ts > o.since \
                 GROUP BY e.rule ORDER BY e.rule",
            )?;
            let it = stmt.query_map([QUICK_OVERWRITE_MS], |r| {
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
                        "{auto_reverted} of {auto_total} auto-fixes undone ({}%)",
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

    /// Open an existing `events.db` for reading only (no migration, no writes). WAL lets
    /// this run next to the orchestrator's writer. Every write method fails on the handle.
    pub fn open_readonly(path: &Path) -> Result<Store> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open {} read-only", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        conn.execute_batch("PRAGMA query_only=ON;")?;
        Ok(Store { conn })
    }

    /// One row per calendar day for the last `days` days (1..=3660), oldest first, today
    /// last; days without events are all zeros. Day boundaries are midnight at
    /// `utc_offset_min` minutes east of UTC (so a browser can pass its own zone).
    pub fn daily_series(&self, days: u32, utc_offset_min: i32) -> Result<Vec<DayStat>> {
        self.daily_series_at(now_ms(), days, utc_offset_min)
    }

    fn daily_series_at(&self, now: i64, days: u32, utc_offset_min: i32) -> Result<Vec<DayStat>> {
        let days = i64::from(days.clamp(1, 3660));
        let offset = i64::from(utc_offset_min.clamp(-1440, 1440)) * 60_000;
        let today = (now + offset).div_euclid(DAY_MS);
        let first = today - (days - 1);
        let since = first * DAY_MS - offset;
        let mut out: Vec<DayStat> = (first..=today)
            .map(|d| {
                let (y, m, dd) = civil_from_days(d);
                DayStat {
                    date: format!("{y:04}-{m:02}-{dd:02}"),
                    shown: 0,
                    accepted: 0,
                    fixes_kept: 0,
                    fixes_reverted: 0,
                    chars_saved: 0,
                }
            })
            .collect();
        let mut stmt = self.conn.prepare_cached(
            "SELECT (ts + ?1) / ?2, \
               COUNT(CASE WHEN source = 'llm_complete' THEN 1 END), \
               COUNT(CASE WHEN source = 'llm_complete' AND outcome = 'accepted' THEN 1 END), \
               COUNT(CASE WHEN source <> 'llm_complete' AND outcome IN ('kept', 'accepted') THEN 1 END), \
               COUNT(CASE WHEN source <> 'llm_complete' AND outcome = 'reverted' THEN 1 END), \
               COALESCE(SUM(CASE \
                 WHEN source = 'llm_complete' AND outcome = 'accepted' THEN length(replacement) \
                 WHEN source <> 'llm_complete' AND outcome = 'kept' THEN length(original) \
                 ELSE 0 END), 0) \
             FROM events WHERE ts >= ?3 GROUP BY 1",
        )?;
        let rows = stmt.query_map(params![offset, DAY_MS, since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                cnt(r.get(1)?),
                cnt(r.get(2)?),
                cnt(r.get(3)?),
                cnt(r.get(4)?),
                cnt(r.get(5)?),
            ))
        })?;
        for row in rows {
            let (day, shown, accepted, kept, reverted, saved) = row?;
            // Events stamped in the future (clock changes) fall outside the window.
            let Some(slot) = usize::try_from(day - first).ok().and_then(|i| out.get_mut(i)) else {
                continue;
            };
            slot.shown = shown;
            slot.accepted = accepted;
            slot.fixes_kept = kept;
            slot.fixes_reverted = reverted;
            slot.chars_saved = saved;
        }
        Ok(out)
    }

    /// Distribution of `latency_ms` over completion events since `since_unix_ms`
    /// (all if `None`), in 20 ms buckets with an open `300+` bucket, plus nearest-rank
    /// p50/p95.
    pub fn latency_histogram(&self, since_unix_ms: Option<i64>) -> Result<LatencyHistogram> {
        let since = since_unix_ms.unwrap_or(i64::MIN);
        let last = (LATENCY_BUCKETS - 1) as i64;
        let mut counts = vec![0u64; LATENCY_BUCKETS];
        let mut stmt = self.conn.prepare_cached(
            "SELECT MIN(MAX(latency_ms, 0) / ?2, ?3), COUNT(*) FROM events \
             WHERE source = 'llm_complete' AND ts >= ?1 GROUP BY 1",
        )?;
        let rows = stmt.query_map(params![since, i64::from(LATENCY_BUCKET_MS), last], |r| {
            Ok((r.get::<_, i64>(0)?, cnt(r.get(1)?)))
        })?;
        for row in rows {
            let (bucket, n) = row?;
            if let Some(slot) = usize::try_from(bucket).ok().and_then(|i| counts.get_mut(i)) {
                *slot = n;
            }
        }
        let total = counts.iter().sum::<u64>();
        let (p50_ms, p95_ms) = if total > 0 {
            (
                completion_percentile(&self.conn, since, total, 50)?,
                completion_percentile(&self.conn, since, total, 95)?,
            )
        } else {
            (0, 0)
        };
        Ok(LatencyHistogram { counts, total, p50_ms, p95_ms })
    }

    /// Counters per app since `since_unix_ms` (all if `None`), busiest first (fixes kept
    /// plus completions accepted, then reverts, then name).
    pub fn by_app(&self, since_unix_ms: Option<i64>) -> Result<Vec<AppStat>> {
        let since = since_unix_ms.unwrap_or(i64::MIN);
        let mut stmt = self.conn.prepare_cached(
            "SELECT MIN(app_exe), \
               COUNT(CASE WHEN source <> 'llm_complete' AND outcome IN ('kept', 'accepted') THEN 1 END), \
               COUNT(CASE WHEN source <> 'llm_complete' AND outcome = 'reverted' THEN 1 END), \
               COUNT(CASE WHEN source = 'llm_complete' THEN 1 END), \
               COUNT(CASE WHEN source = 'llm_complete' AND outcome = 'accepted' THEN 1 END) \
             FROM events WHERE ts >= ?1 GROUP BY app_exe COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok(AppStat {
                app_exe: r.get(0)?,
                fixes_kept: cnt(r.get(1)?),
                fixes_reverted: cnt(r.get(2)?),
                shown: cnt(r.get(3)?),
                accepted: cnt(r.get(4)?),
            })
        })?;
        let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        out.sort_by(|a, b| {
            (b.fixes_kept + b.accepted)
                .cmp(&(a.fixes_kept + a.accepted))
                .then(b.fixes_reverted.cmp(&a.fixes_reverted))
                .then_with(|| a.app_exe.to_lowercase().cmp(&b.app_exe.to_lowercase()))
        });
        Ok(out)
    }

    /// Completions shown and accepted per confidence decile since `since_unix_ms` (all if
    /// `None`). Always [`CONFIDENCE_BUCKETS`] rows, lowest confidence first.
    pub fn by_confidence(&self, since_unix_ms: Option<i64>) -> Result<Vec<ConfidenceBucket>> {
        let since = since_unix_ms.unwrap_or(i64::MIN);
        let last = (CONFIDENCE_BUCKETS - 1) as i64;
        let mut out: Vec<ConfidenceBucket> = (0..CONFIDENCE_BUCKETS as u32)
            .map(|bucket| ConfidenceBucket { bucket, shown: 0, accepted: 0 })
            .collect();
        // The epsilon keeps 0.7 stored as the f32 0.69999999 in the 0.7 decile.
        let mut stmt = self.conn.prepare_cached(
            "SELECT MIN(CAST(confidence * 10 + 0.0001 AS INTEGER), ?2), COUNT(*), \
               COUNT(CASE WHEN outcome = 'accepted' THEN 1 END) \
             FROM events WHERE source = 'llm_complete' AND ts >= ?1 GROUP BY 1",
        )?;
        let rows = stmt.query_map(params![since, last], |r| {
            Ok((r.get::<_, i64>(0)?, cnt(r.get(1)?), cnt(r.get(2)?)))
        })?;
        for row in rows {
            let (bucket, shown, accepted) = row?;
            if let Some(slot) = usize::try_from(bucket).ok().and_then(|i| out.get_mut(i)) {
                slot.shown = shown;
                slot.accepted = accepted;
            }
        }
        Ok(out)
    }

    /// The newest `limit` events (at most 1000) of `kind`, newest first.
    pub fn recent_events(&self, limit: usize, kind: EventKind) -> Result<Vec<EventRow>> {
        let limit = i64::try_from(limit.min(1000)).unwrap_or(1000);
        let kind = match kind {
            EventKind::All => 0i64,
            EventKind::Fix => 1,
            EventKind::Complete => 2,
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, ts, app_exe, source, rule, severity, confidence, context_before, \
               original, replacement, action, outcome, latency_ms \
             FROM events \
             WHERE ?2 = 0 OR (?2 = 1 AND source <> 'llm_complete') \
                OR (?2 = 2 AND source = 'llm_complete') \
             ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit, kind], |r| {
            Ok(EventRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                app_exe: r.get(2)?,
                source: r.get(3)?,
                rule: r.get(4)?,
                severity: r.get(5)?,
                confidence: r.get(6)?,
                context_before: r.get(7)?,
                original: r.get(8)?,
                replacement: r.get(9)?,
                action: r.get(10)?,
                outcome: r.get(11)?,
                latency_ms: u32::try_from(r.get::<_, i64>(12)?.max(0)).unwrap_or(u32::MAX),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// `(typed, fix)` pairs, lowercase, of auto-fixes the user rejects: undone (reverted, or
    /// backspaced over within 3 s) and never kept, or undone three times and at least as
    /// often as kept.
    pub fn undone_pairs(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT lower(original), lower(replacement), \
               COUNT(CASE WHEN outcome = 'reverted' \
                 OR (outcome = 'overwritten' AND outcome_ts - ts <= ?1) THEN 1 END) AS undone, \
               COUNT(CASE WHEN outcome = 'kept' THEN 1 END) AS kept \
             FROM events WHERE action = 'auto' AND source <> 'llm_complete' \
             GROUP BY 1, 2 HAVING (undone >= 1 AND kept = 0) OR (undone >= 3 AND undone >= kept)",
        )?;
        let rows = stmt.query_map([QUICK_OVERWRITE_MS], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Records whether the auto-fix `event_id` read back from the field as typed.
    pub fn log_fix_check(&self, event_id: i64, app_exe: &str, landed: bool) -> Result<()> {
        self.conn
            .prepare_cached("INSERT INTO fix_checks(event_id, ts, app_exe, landed) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![event_id, now_ms(), app_exe, landed])?;
        Ok(())
    }

    /// The newest [`LANDING_WINDOW`] fix checks in `app_exe` from the last week.
    pub fn landing(&self, app_exe: &str) -> Result<Landing> {
        let since = now_ms() - LANDING_MAX_AGE_MS;
        let (checked, failed) = self.conn.prepare_cached(
            "SELECT COUNT(*), COUNT(CASE WHEN landed = 0 THEN 1 END) FROM ( \
               SELECT landed FROM fix_checks WHERE app_exe = ?1 COLLATE NOCASE AND ts >= ?2 \
               ORDER BY ts DESC LIMIT ?3)",
        )?
        .query_row(params![app_exe, since, LANDING_WINDOW], |r| Ok((cnt(r.get(0)?), cnt(r.get(1)?))))?;
        Ok(Landing { checked, failed })
    }

    /// Rules currently capped at "suggest" (see [`Store::tune`]), sorted by name.
    pub fn demoted_rules(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT rule FROM rule_overrides WHERE max_severity = 'suggest' ORDER BY rule",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    tx.execute_batch(SCHEMA_ADDED)?;
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

/// Nearest-rank percentile of `latency_ms` among the `n` completion events since `since`.
fn completion_percentile(conn: &Connection, since: i64, n: u64, p: u64) -> Result<u32> {
    let rank = (n * p).div_ceil(100).max(1); // 1-based
    let offset = i64::try_from(rank - 1).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare_cached(
        "SELECT latency_ms FROM events WHERE source = 'llm_complete' AND ts >= ?1 \
         ORDER BY latency_ms LIMIT 1 OFFSET ?2",
    )?;
    let v = match stmt.query_row(params![since, offset], |r| r.get::<_, i64>(0)) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => 0,
        Err(e) => return Err(e.into()),
    };
    Ok(u32::try_from(v.max(0)).unwrap_or(u32::MAX))
}

/// Proleptic Gregorian (year, month, day) of the day `days` days after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
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
    fn undone_pairs_are_fixes_the_user_rejects() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        let auto = |typed: &str, fixed: &str, outcome: Outcome| {
            let id = s.log(&fix("spell", Action::Auto, typed, fixed)).unwrap();
            s.set_outcome(id, outcome).unwrap();
            id
        };
        auto("ok", "k", Outcome::Overwritten);
        auto("thhe", "the", Outcome::Kept);
        auto("thhe", "the", Outcome::Overwritten);
        auto("Tst", "test", Outcome::Reverted);
        let late = auto("ender", "under", Outcome::Overwritten);
        s.conn.execute("UPDATE events SET outcome_ts = ts + 9000 WHERE id = ?1", [late]).unwrap();
        let mut pairs = s.undone_pairs().unwrap();
        pairs.sort();
        assert_eq!(pairs, [("ok".to_string(), "k".to_string()), ("tst".to_string(), "test".to_string())]);
    }

    #[test]
    fn tune_counts_a_quick_overwrite_as_undone() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        for i in 0..20 {
            let id = s.log(&fix("r", Action::Auto, "teh", "the")).unwrap();
            s.set_outcome(id, if i < 6 { Outcome::Overwritten } else { Outcome::Kept }).unwrap();
        }
        let t = s.tune().unwrap();
        assert!(matches!(&t[..], [Tuning::Demote { rule, reason }] if rule == "r" && reason.contains("6 of 20")), "{t:?}");
        assert_eq!(s.rule_stats(0).unwrap()[0].auto_reverted, 6);
    }

    #[test]
    fn landing_counts_the_latest_week_of_checks_per_app() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        for i in 0..25 {
            s.log_fix_check(i, "Claude.exe", i % 10 != 0).unwrap();
        }
        s.log_fix_check(99, "notepad.exe", true).unwrap();
        let l = s.landing("claude.exe").unwrap();
        assert_eq!((l.checked, l.failed), (25, 3));
        assert!(l.too_low(), "3 of 25 is more than one in fifty");
        assert_eq!(s.landing("notepad.exe").unwrap(), Landing { checked: 1, failed: 0 });
        assert!(!Landing { checked: 19, failed: 19 }.too_low(), "too few to tell");
        assert!(!Landing { checked: 50, failed: 1 }.too_low());
        s.conn.execute("UPDATE fix_checks SET ts = ts - 8 * 86400000 WHERE app_exe = 'Claude.exe'", []).unwrap();
        assert_eq!(s.landing("claude.exe").unwrap().checked, 0, "a week later the app starts over");
    }

    #[test]
    fn landing_keeps_only_the_newest_checks() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        for i in 0..10 {
            s.log_fix_check(i, "a.exe", false).unwrap();
        }
        s.conn.execute("UPDATE fix_checks SET ts = ts - 1000", []).unwrap();
        for i in 10..60 {
            s.log_fix_check(i, "a.exe", true).unwrap();
        }
        assert_eq!(s.landing("a.exe").unwrap(), Landing { checked: 50, failed: 0 });
    }

    #[test]
    fn an_older_build_can_open_the_file_with_the_added_tables() {
        let dir = TempDir::new();
        let path = dir.db();
        Store::open(&path).unwrap().log_fix_check(1, "a.exe", true).unwrap();
        let s = Store::open(&path).unwrap();
        let v: i64 = s.conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 1);
        assert_eq!(s.landing("a.exe").unwrap().checked, 1);
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

    fn event_in(app: &str, source: Source, action: Action, original: &str, replacement: &str) -> NewEvent {
        NewEvent {
            app_exe: app.into(),
            source,
            rule: if source == Source::LlmComplete { "complete" } else { "spell" }.into(),
            severity: "S1".into(),
            confidence: 0.5,
            context_before: None,
            original: original.into(),
            replacement: replacement.into(),
            action,
            latency_ms: 10,
        }
    }

    /// A completion with the given confidence and latency, logged now.
    fn complete_with(s: &Store, app: &str, conf: f32, latency_ms: u32, outcome: Option<Outcome>) -> i64 {
        let mut ev = event_in(app, Source::LlmComplete, Action::Suggested, "", "some text");
        ev.confidence = conf;
        ev.latency_ms = latency_ms;
        let id = s.log(&ev).unwrap();
        if let Some(o) = outcome {
            s.set_outcome(id, o).unwrap();
        }
        id
    }

    #[test]
    fn open_readonly_reads_but_cannot_write() {
        let dir = TempDir::new();
        let path = dir.db();
        let rw = Store::open(&path).unwrap();
        let id = rw.log(&fix("spell", Action::Auto, "teh", "the")).unwrap();
        rw.set_outcome(id, Outcome::Kept).unwrap();

        let ro = Store::open_readonly(&path).unwrap();
        let m = ro.metrics(None).unwrap();
        assert_eq!((m.events, m.fixes_auto, m.chars_saved), (1, 1, 3));
        assert!(ro.log(&fix("spell", Action::Auto, "adn", "and")).is_err());
        assert!(ro.set_outcome(id, Outcome::Reverted).is_err());
        assert!(ro.expire_pending(0).is_err());
        // A writer keeps working and the reader sees its later rows.
        rw.log(&fix("spell", Action::Auto, "adn", "and")).unwrap();
        assert_eq!(ro.metrics(None).unwrap().events, 2);
        drop((ro, rw));

        assert!(Store::open_readonly(&dir.0.join("missing.db")).is_err());
        // Also fine when the writer has closed and the WAL files are gone.
        assert_eq!(Store::open_readonly(&path).unwrap().metrics(None).unwrap().events, 2);
    }

    #[test]
    fn civil_dates_round_trip_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(100), (1970, 4, 11));
    }

    #[test]
    fn daily_series_buckets_by_day_and_offset() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        let hour = 3_600_000;
        let now = 100 * DAY_MS + 12 * hour; // 1970-04-11 12:00 UTC

        let at = |ev: NewEvent, ts: i64, o: Option<Outcome>| {
            let id = s.log(&ev).unwrap();
            set_ts(&s, id, ts);
            if let Some(o) = o {
                s.set_outcome(id, o).unwrap();
            }
        };
        let auto = |orig: &str, rep: &str| event_in("a.exe", Source::Harper, Action::Auto, orig, rep);
        let comp = || event_in("a.exe", Source::LlmComplete, Action::Suggested, "", "hello world");

        at(auto("teh", "the"), 100 * DAY_MS + 10 * hour, Some(Outcome::Kept));
        at(
            event_in("a.exe", Source::Harper, Action::Suggested, "wierd", "weird"),
            100 * DAY_MS + hour,
            Some(Outcome::Accepted),
        );
        at(auto("adn", "and"), 99 * DAY_MS + 23 * hour + hour / 2, Some(Outcome::Reverted));
        at(comp(), 98 * DAY_MS + 5 * hour, Some(Outcome::Accepted));
        at(comp(), 98 * DAY_MS + 6 * hour, Some(Outcome::Ignored));
        at(auto("old", "older"), 90 * DAY_MS, Some(Outcome::Kept)); // before the window
        at(auto("fut", "future"), 102 * DAY_MS + hour, Some(Outcome::Kept)); // after today

        let utc = s.daily_series_at(now, 3, 0).unwrap();
        let dates: Vec<_> = utc.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(dates, ["1970-04-09", "1970-04-10", "1970-04-11"]);
        assert_eq!(
            utc[0],
            DayStat {
                date: "1970-04-09".into(),
                shown: 2,
                accepted: 1,
                fixes_kept: 0,
                fixes_reverted: 0,
                chars_saved: 11
            }
        );
        assert_eq!((utc[1].fixes_reverted, utc[1].fixes_kept, utc[1].chars_saved), (1, 0, 0));
        // Kept auto fix (3 chars of "teh") and an accepted suggestion; only the former saves chars.
        assert_eq!((utc[2].fixes_kept, utc[2].fixes_reverted, utc[2].chars_saved), (2, 0, 3));

        // UTC+1: the revert at 23:30 UTC on the 10th is 00:30 on the 11th locally.
        let plus1 = s.daily_series_at(now, 3, 60).unwrap();
        assert_eq!((plus1[1].fixes_reverted, plus1[2].fixes_reverted), (0, 1));
        assert_eq!(plus1[2].fixes_kept, 2);
        // UTC-12: 12:00 UTC is 00:00 on the 11th, so today starts now and holds nothing yet
        // (the kept fix at 10:00 UTC and the suggestion at 01:00 UTC are on the 10th there).
        let minus12 = s.daily_series_at(now, 1, -12 * 60).unwrap();
        assert_eq!(minus12.len(), 1);
        assert_eq!(minus12[0].date, "1970-04-11");
        assert_eq!(minus12[0].fixes_kept, 0);
    }

    #[test]
    fn daily_series_zero_fills_and_clamps() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        assert_eq!(s.daily_series(0, 0).unwrap().len(), 1);
        let rows = s.daily_series(30, 0).unwrap();
        assert_eq!(rows.len(), 30);
        assert!(rows.iter().all(|d| d.shown + d.accepted + d.fixes_kept + d.fixes_reverted + d.chars_saved == 0));
        assert!(rows.windows(2).all(|w| w[0].date < w[1].date));
        assert_eq!(s.daily_series(1_000_000, 0).unwrap().len(), 3660);
    }

    #[test]
    fn latency_histogram_buckets_and_percentiles() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        let empty = s.latency_histogram(None).unwrap();
        assert_eq!((empty.total, empty.p50_ms, empty.p95_ms), (0, 0, 0));
        assert_eq!(empty.counts.len(), LATENCY_BUCKETS);

        for ms in [5, 19, 20, 45, 100, 299, 300, 900] {
            complete_with(&s, "a.exe", 0.5, ms, None);
        }
        // A fix with a huge latency must not count.
        let mut slow = fix("spell", Action::Auto, "teh", "the");
        slow.latency_ms = 5000;
        s.log(&slow).unwrap();
        // An old completion is excluded by `since`.
        let old = complete_with(&s, "a.exe", 0.5, 150, None);
        set_ts(&s, old, 1_000);

        let h = s.latency_histogram(Some(2_000)).unwrap();
        let mut want = vec![0u64; LATENCY_BUCKETS];
        want[0] = 2;
        want[1] = 1;
        want[2] = 1;
        want[5] = 1;
        want[14] = 1;
        want[15] = 2;
        assert_eq!(h.counts, want);
        assert_eq!((h.total, h.p50_ms, h.p95_ms), (8, 45, 900));

        let all = s.latency_histogram(None).unwrap();
        assert_eq!(all.total, 9);
        assert_eq!(all.counts[7], 1);
    }

    #[test]
    fn by_app_groups_case_insensitively_and_orders_by_activity() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        let put = |app: &str, o: Outcome| {
            let id = s
                .log(&event_in(app, Source::Harper, Action::Auto, "teh", "the"))
                .unwrap();
            s.set_outcome(id, o).unwrap();
        };
        put("Notepad.exe", Outcome::Kept);
        put("notepad.exe", Outcome::Kept);
        put("NOTEPAD.EXE", Outcome::Reverted);
        complete_with(&s, "notepad.exe", 0.4, 50, Some(Outcome::Accepted));
        complete_with(&s, "code.exe", 0.4, 50, Some(Outcome::Ignored));
        complete_with(&s, "code.exe", 0.4, 50, None);
        let old = complete_with(&s, "slack.exe", 0.4, 50, Some(Outcome::Accepted));
        set_ts(&s, old, 1_000);

        let apps = s.by_app(Some(2_000)).unwrap();
        assert_eq!(apps.len(), 2, "{apps:?}");
        assert_eq!(apps[0].app_exe.to_lowercase(), "notepad.exe");
        assert_eq!(
            (apps[0].fixes_kept, apps[0].fixes_reverted, apps[0].shown, apps[0].accepted),
            (2, 1, 1, 1)
        );
        assert_eq!(apps[1].app_exe, "code.exe");
        assert_eq!((apps[1].fixes_kept, apps[1].shown, apps[1].accepted), (0, 2, 0));
        assert_eq!(s.by_app(None).unwrap().len(), 3);
    }

    #[test]
    fn by_confidence_always_returns_ten_deciles() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        let none = s.by_confidence(None).unwrap();
        assert_eq!(none.len(), CONFIDENCE_BUCKETS);
        assert!(none.iter().enumerate().all(|(i, b)| b.bucket == i as u32 && b.shown == 0));

        complete_with(&s, "a.exe", 0.0, 10, Some(Outcome::Ignored));
        complete_with(&s, "a.exe", 0.05, 10, Some(Outcome::Accepted));
        complete_with(&s, "a.exe", 0.1, 10, None);
        complete_with(&s, "a.exe", 0.7, 10, Some(Outcome::Accepted)); // f32 0.7 < 0.7 exactly
        complete_with(&s, "a.exe", 0.99, 10, Some(Outcome::Accepted));
        complete_with(&s, "a.exe", 1.0, 10, Some(Outcome::Overwritten));
        // Fixes never count.
        s.log(&fix("spell", Action::Auto, "teh", "the")).unwrap();

        let b = s.by_confidence(None).unwrap();
        let shown: Vec<u64> = b.iter().map(|x| x.shown).collect();
        let accepted: Vec<u64> = b.iter().map(|x| x.accepted).collect();
        assert_eq!(shown, [2, 1, 0, 0, 0, 0, 0, 1, 0, 2]);
        assert_eq!(accepted, [1, 0, 0, 0, 0, 0, 0, 1, 0, 1]);
    }

    #[test]
    fn recent_events_newest_first_and_filtered() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        assert!(s.recent_events(10, EventKind::All).unwrap().is_empty());

        let a = s.log(&fix("spell", Action::Auto, "teh", "the")).unwrap();
        let b = complete_with(&s, "code.exe", 0.4, 77, Some(Outcome::Accepted));
        let c = s.log(&fix("grammar", Action::Suggested, "wierd", "weird")).unwrap();
        for (id, ts) in [(a, 1_000), (b, 2_000), (c, 3_000)] {
            set_ts(&s, id, ts);
        }
        s.set_outcome(a, Outcome::Kept).unwrap();

        let all = s.recent_events(10, EventKind::All).unwrap();
        assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), [c, b, a]);
        assert_eq!(s.recent_events(2, EventKind::All).unwrap().len(), 2);
        assert!(s.recent_events(0, EventKind::All).unwrap().is_empty());

        let fixes = s.recent_events(10, EventKind::Fix).unwrap();
        assert_eq!(fixes.iter().map(|e| e.id).collect::<Vec<_>>(), [c, a]);
        assert_eq!(fixes[1].outcome.as_deref(), Some("kept"));
        assert_eq!((fixes[1].original.as_str(), fixes[1].replacement.as_str()), ("teh", "the"));
        assert_eq!(fixes[0].outcome, None);

        let comps = s.recent_events(10, EventKind::Complete).unwrap();
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].source, "llm_complete");
        assert_eq!((comps[0].app_exe.as_str(), comps[0].latency_ms), ("code.exe", 77));
        assert_eq!(comps[0].outcome.as_deref(), Some("accepted"));
    }

    #[test]
    fn demoted_rules_lists_suggest_overrides() {
        let dir = TempDir::new();
        let s = Store::open(&dir.db()).unwrap();
        assert!(s.demoted_rules().unwrap().is_empty());
        log_auto(&s, "bad_rule", 25, 8);
        log_auto(&s, "fine_rule", 25, 0);
        s.tune().unwrap();
        assert_eq!(s.demoted_rules().unwrap(), ["bad_rule"]);
        s.clear_demotion("bad_rule").unwrap();
        assert!(s.demoted_rules().unwrap().is_empty());
    }
}
