//! CaseComplete orchestrator. Wires hook -> context -> engine -> overlay -> inject.
#![windows_subsystem = "windows"]
//!
//! One orchestrator thread owns all state and receives every event on one channel:
//! keys from the hook thread, streamed tokens from the engine runtime, and a periodic tick.
//! The hook thread never does work; the engine runs on its own tokio runtime; UIA calls
//! happen only on the orchestrator thread (which calls `cc_context::init_thread`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

mod autostart;
mod privacy;
use privacy::Policy;

use cc_ui::{ContextSnapshot, ModelInfo, UiCommand, UiServer, UiSettings, UiState};
use cc_window::Windows;

use anyhow::Context;
use cc_correct::{AutocorrectList, Checker, PersonalDictionary, Severity};
use cc_engine::{Completion, CompletionClient, Sidecar};
use cc_hook::{HookEvent, ShadowBuffer, SwallowState};
use cc_overlay::{GhostStyle, Overlay};
use cc_store::{Action, NewEvent, Outcome, Source, Store};
use cc_tray::{Tray, TrayEvent, TrayState};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use tokio_util::sync::CancellationToken;
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_ICONWARNING, MB_OK, MESSAGEBOX_STYLE};

const PORT: u16 = 8089;
const DEBOUNCE: Duration = Duration::from_millis(100);
const N_PREDICT: u32 = 24;
const CONTEXT_CHARS: usize = 800;
const REVERT_WINDOW_MS: u64 = 3000;
/// A fix nobody touched for this long counts as kept.
const KEPT_AFTER: Duration = Duration::from_secs(10);
const TUNE_EVERY: Duration = Duration::from_secs(6 * 3600);
const STATUS_EVERY: Duration = Duration::from_secs(60);
const STYLE_EVERY: Duration = Duration::from_secs(300);
const UNDERLINE_FADE_MS: u32 = 1500;
const DEFAULT_MODEL: &str = "gemma-3-1b-pt";
/// Terminals: UIA reports the whole screen buffer, not an edit field, so the shadow buffer
/// is the only context and editability is assumed.
const TERMINAL_EXES: &[&str] = &["windowsterminal.exe", "conhost.exe", "openconsole.exe", "wezterm-gui.exe", "alacritty.exe"];

enum Msg {
    Key(HookEvent),
    Token { gen: u64, text: String, logprob: Option<f32> },
    Done { gen: u64, result: anyhow::Result<Completion> },
}

struct Req {
    gen: u64,
    prompt: String,
    cancel: CancellationToken,
}

/// Engine runtime: receives prompts, streams tokens back as `Msg`.
fn spawn_engine(client: CompletionClient, out: Sender<Msg>) -> tokio::sync::mpsc::UnboundedSender<Req> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Req>();
    std::thread::Builder::new()
        .name("cc-engine".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                while let Some(req) = rx.recv().await {
                    let client = client.clone();
                    let out = out.clone();
                    tokio::spawn(async move {
                        let gen = req.gen;
                        let out_tok = out.clone();
                        let result = client
                            .complete_with_probs(&req.prompt, N_PREDICT, req.cancel, move |tok, logprob| {
                                let _ = out_tok.send(Msg::Token { gen, text: tok.to_string(), logprob });
                            })
                            .await;
                        let _ = out.send(Msg::Done { gen, result });
                    });
                }
            });
        })
        .expect("spawn engine thread");
    tx
}

/// The last auto-applied correction, revertable while the hook's revert window is open.
struct Fix {
    original: String,
    replacement: String,
    boundary: char,
    event_id: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    enabled: bool,
    auto_fix_level: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_min_confidence")]
    min_confidence: f32,
}

fn default_min_confidence() -> f32 {
    0.12
}

fn default_model() -> String {
    DEFAULT_MODEL.into()
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: true, auto_fix_level: "S1".into(), model: default_model(), min_confidence: 0.12 }
    }
}

fn parse_level(s: &str) -> Option<Severity> {
    match s {
        "off" => None,
        "S0" => Some(Severity::S0),
        "S2" => Some(Severity::S2),
        _ => Some(Severity::S1),
    }
}

fn level_name(level: &Option<Severity>) -> String {
    match level {
        None => "off".into(),
        Some(Severity::S0) => "S0".into(),
        Some(Severity::S1) => "S1".into(),
        Some(_) => "S2".into(),
    }
}

struct App {
    checker: Checker,
    dict: PersonalDictionary,
    dict_path: PathBuf,
    auto: AutocorrectList,
    auto_path: PathBuf,
    /// The last word closed by a boundary and the shadow length right after it, used to
    /// notice when the user goes back and retypes that word differently.
    prev_boundary: Option<(String, usize)>,
    auto_level: Option<Severity>,
    last_fix: Option<Fix>,
    store: Store,
    tray: Tray,
    profile: PathBuf,
    root: PathBuf,
    policy: Policy,
    model_id: String,
    rt: tokio::runtime::Runtime,
    sidecar: Option<Sidecar>,
    denied_hwnd: isize,
    ui_state: Arc<UiState>,
    ui: UiServer,
    ui_cmds: Receiver<UiCommand>,
    windows: Windows,
    /// Tail of the prompt sent for the current completion, for the context panel.
    last_prompt_tail: String,
    enabled: bool,
    paused_until: Option<Instant>,
    /// Rules the nightly tuning demoted to suggest-only.
    demoted: HashSet<String>,
    /// (event id, applied at, shadow length right after) of the last auto fix awaiting an outcome.
    pending_fix_event: Option<(i64, Instant, usize)>,
    pending_completion_event: Option<i64>,
    completion_latency_ms: u32,
    /// Raw streamed text of the current completion, before healing strips the partial word.
    raw: String,
    /// The half-typed word the prompt was cut before; the completion must start with it.
    heal: String,
    /// The prompt was trimmed of trailing spaces the user already typed, so the completion
    /// must begin with whitespace and the ghost shows it without.
    need_space: bool,
    last_prompt_ms: f32,
    last_context_event: Option<i64>,
    tok_count: u32,
    logprob_sum: f32,
    min_conf: f32,
    style_lines: Vec<String>,
    style_refreshed: Instant,
    last_tune: Instant,
    last_status: Instant,
    /// Keys the hook swallowed during an injection, replayed once the channel has drained.
    replay: String,
    replay_at: Option<Instant>,
    dont_touch: HashSet<String>,
    reverts: HashMap<String, u32>,
    shadow: ShadowBuffer,
    ghost: String,
    ghost_visible: bool,
    gen: u64,
    cancel: CancellationToken,
    pending: bool,
    last_key: Instant,
    req_started: Option<Instant>,
    first_token_logged: bool,
    swallow: Arc<SwallowState>,
    overlay: Overlay,
    engine: tokio::sync::mpsc::UnboundedSender<Req>,
    style: GhostStyle,
    last_exe: String,
    last_hwnd: isize,
    in_terminal: bool,
}

impl App {
    fn active(&self) -> bool {
        self.enabled && self.paused_until.is_none_or(|t| Instant::now() >= t)
    }

    fn settings_path(&self) -> PathBuf {
        self.profile.join("settings.json")
    }

    fn save_settings(&self) {
        let s = Settings {
            enabled: self.enabled,
            auto_fix_level: level_name(&self.auto_level),
            model: self.model_id.clone(),
            min_confidence: self.min_conf,
        };
        self.push_ui_settings();
        if let Ok(json) = serde_json::to_string_pretty(&s) {
            if let Err(e) = std::fs::write(self.settings_path(), json) {
                log::warn!("settings save failed: {e}");
            }
        }
    }

    fn outcome(&mut self, id: i64, outcome: Outcome) {
        if let Err(e) = self.store.set_outcome(id, outcome) {
            log::warn!("store outcome failed: {e:#}");
        }
        if Some(id) == self.last_context_event {
            if let Ok(mut c) = self.ui_state.last_context.lock() {
                if let Some(c) = c.as_mut() {
                    c.outcome = Some(format!("{outcome:?}").to_lowercase());
                }
            }
        }
    }

    /// Mirrors the live settings into the web UI's shared state.
    fn push_ui_settings(&self) {
        let models = cc_engine::MODELS
            .iter()
            .map(|m| ModelInfo {
                id: m.id.to_string(),
                label: m.label.to_string(),
                origin: m.origin.to_string(),
                installed: cc_engine::model_installed(&self.root, m.id),
            })
            .collect();
        let paused_until_ms = self.paused_until.and_then(|t| {
            let left = t.checked_duration_since(Instant::now())?;
            Some(unix_ms() + left.as_millis() as i64)
        });
        if let Ok(mut s) = self.ui_state.settings.lock() {
            let status_line = std::mem::take(&mut s.status_line);
            *s = UiSettings {
                enabled: self.enabled,
                paused_until_ms,
                auto_fix_level: level_name(&self.auto_level),
                model: self.model_id.clone(),
                min_confidence: self.min_conf,
                models,
                profile_dir: self.profile.display().to_string(),
                autostart: autostart::is_enabled(),
                status_line,
            };
        }
    }

    /// One line per block in the profile's privacy_log.jsonl; only the reason, never the text.
    fn log_privacy(&self, reason: &str) {
        use std::io::Write;
        let line = format!("{{\"ts\":{},\"reason\":{}}}\n", unix_ms(), serde_json::to_string(reason).unwrap_or_default());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.profile.join("privacy_log.jsonl")) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn record_context(&mut self, completion: &str, confidence: f32, privacy_block: Option<String>) {
        let snap = ContextSnapshot {
            ts_ms: unix_ms(),
            app_exe: self.last_exe.clone(),
            style_lines: self.style_lines.clone(),
            prompt_tail: self.last_prompt_tail.clone(),
            heal: self.heal.clone(),
            need_space: self.need_space,
            completion: completion.to_string(),
            confidence,
            tokens: self.tok_count,
            latency_ms: self.completion_latency_ms,
            prompt_ms: self.last_prompt_ms,
            outcome: None,
            privacy_block,
        };
        if let Ok(mut c) = self.ui_state.last_context.lock() {
            *c = Some(snap);
        }
    }

    fn on_ui_command(&mut self, cmd: UiCommand) {
        log::info!("ui: {cmd:?}");
        match cmd {
            UiCommand::SetEnabled(b) => self.on_tray(TrayEvent::Enabled(b)),
            UiCommand::PauseMinutes(m) => self.on_tray(TrayEvent::PauseMinutes(m)),
            UiCommand::SetAutoFixLevel(s) => self.on_tray(TrayEvent::AutoFixLevel(s)),
            UiCommand::SetModel(id) => self.on_tray(TrayEvent::Model(id)),
            UiCommand::SetMinConfidence(c) => {
                self.min_conf = c.clamp(0.0, 1.0);
                self.save_settings();
            }
            UiCommand::AddDictionaryWord(w) => {
                self.dict.add(&w);
                let _ = self.dict.save(&self.dict_path);
            }
            UiCommand::RemoveDictionaryWord(w) => {
                self.dict = PersonalDictionary::load(&self.dict_path);
                let keep: Vec<String> = std::fs::read_to_string(&self.dict_path)
                    .unwrap_or_default()
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case(&w))
                    .map(String::from)
                    .collect();
                let _ = std::fs::write(&self.dict_path, keep.join("\n") + "\n");
                self.dict = PersonalDictionary::load(&self.dict_path);
            }
            UiCommand::SetAutocorrect(typo, fix) => {
                self.auto.add(&typo, &fix);
                let _ = self.auto.save(&self.auto_path);
            }
            UiCommand::RemoveAutocorrect(typo) => {
                self.auto.remove(&typo);
                let _ = self.auto.save(&self.auto_path);
            }
            UiCommand::SetAutostart(b) => {
                if let Err(e) = autostart::set(b) {
                    log::warn!("autostart: {e:#}");
                }
                self.push_ui_settings();
            }
            UiCommand::OpenProfileFolder => self.on_tray(TrayEvent::OpenProfileFolder),
            UiCommand::OpenDashboard(page) => {
                self.windows.close_flyout();
                let view = if page.is_empty() { "dashboard".to_string() } else { format!("dashboard/{page}") };
                self.windows.open_dashboard(&self.ui.url(&view));
            }
            UiCommand::Quit => self.on_tray(TrayEvent::Quit),
        }
    }

    fn cancel_ghost(&mut self) {
        if let Some(id) = self.pending_completion_event.take() {
            self.outcome(id, Outcome::Ignored);
        }
        if !self.ghost.is_empty() || self.ghost_visible {
            self.overlay.hide();
        }
        self.ghost.clear();
        self.ghost_visible = false;
        self.swallow.set_ghost_visible(false);
        self.cancel.cancel();
        self.cancel = CancellationToken::new();
    }

    fn resync(&mut self) {
        self.cancel_ghost();
        self.pending = false;
        if self.in_terminal {
            self.shadow.reset("");
            return;
        }
        if let Some(t) = cc_context::text_before_caret(CONTEXT_CHARS) {
            self.shadow.reset(&t);
        } else {
            self.shadow.reset("");
        }
    }

    fn field_allowed(&mut self) -> bool {
        match cc_context::focus_info() {
            Some(fi) => {
                if !self.active() {
                    return false;
                }
                if fi.hwnd != self.last_hwnd {
                    log::info!("focus: {} editable={} password={}", fi.exe, fi.is_editable, fi.is_password);
                    self.last_exe = fi.exe.clone();
                    self.last_hwnd = fi.hwnd;
                    // A different window means the shadow buffer describes the wrong field.
                    self.shadow.reset("");
                }
                self.in_terminal = TERMINAL_EXES.contains(&fi.exe.as_str());
                if let Some(why) = self.policy.denies(&fi.exe, &cc_context::window_title()) {
                    if self.denied_hwnd != fi.hwnd {
                        self.denied_hwnd = fi.hwnd;
                        log::info!("privacy: off here ({why})");
                        self.log_privacy(&why);
                    }
                    return false;
                }
                !fi.is_password && (fi.is_editable || self.in_terminal)
            }
            None => false,
        }
    }

    fn request_completion(&mut self) {
        self.pending = false;
        if !self.field_allowed() {
            return;
        }
        // UIA text is authoritative once it has caught up with what we typed or injected;
        // right after an injection it can still be short, so the shadow buffer wins then.
        if !self.in_terminal {
            if let Some(t) = cc_context::text_before_caret(CONTEXT_CHARS) {
                if t.len() >= self.shadow.text().len() {
                    self.shadow.reset(&t);
                }
            }
        }
        let ctx = self.shadow.text().to_string();
        if ctx.trim().split_whitespace().count() < 2 {
            return;
        }
        if let Some(why) = privacy::sensitive(&ctx) {
            log::debug!("privacy: no completion ({why})");
            self.log_privacy(why);
            self.last_prompt_tail.clear();
            self.record_context("", 0.0, Some(why.to_string()));
            return;
        }
        self.cancel_ghost();
        // Token healing: cut the prompt before a half-typed word so the model completes a
        // whole word; the stream is then required to start with what was typed.
        let partial = self.shadow.current_word().to_string();
        let heal = if partial.chars().all(|c| c.is_alphanumeric() || c == '\'') && !partial.is_empty() {
            partial
        } else {
            String::new()
        };
        // A prompt that ends in a space makes small base models answer with digits (bench:
        // top-1 drops from 13% to ~1%), so the prompt always ends at a word and the
        // completion must supply the space itself.
        let cut = ctx[..ctx.len() - heal.len()].to_string();
        let trimmed = cut.trim_end_matches(' ');
        self.need_space = trimmed.len() < cut.len() && !trimmed.is_empty() && !trimmed.ends_with('\n');
        let ctx = trimmed.to_string();
        self.heal = heal;
        self.raw.clear();
        self.tok_count = 0;
        self.logprob_sum = 0.0;
        // Recently accepted sentences in this app give the base model your voice.
        self.last_prompt_tail = ctx.chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
        let ctx = if self.style_lines.is_empty() { ctx } else { format!("{}\n\n{}", self.style_lines.join("\n"), ctx) };
        self.gen += 1;
        self.req_started = Some(Instant::now());
        self.first_token_logged = false;
        let _ = self.engine.send(Req { gen: self.gen, prompt: ctx, cancel: self.cancel.clone() });
    }

    fn show_ghost(&mut self) {
        let Some(rect) = cc_context::caret_rect() else {
            log::debug!("no caret rect; ghost hidden");
            return;
        };
        if let Some((family, px)) = cc_context::font_at_caret() {
            self.style.font_family = family;
            self.style.font_px = px;
        } else {
            self.style.font_px = (rect.h as f32 * 0.8).clamp(11.0, 40.0);
        }
        self.overlay.show_ghost(&self.ghost, rect.x + rect.w, rect.y, &self.style);
        if !self.ghost_visible {
            self.ghost_visible = true;
            self.swallow.set_ghost_visible(true);
        }
    }

    fn accept(&mut self, whole: bool) {
        if self.ghost.is_empty() {
            return;
        }
        // Hide first so the hook stops swallowing Tab/Esc while the text goes in.
        self.overlay.hide();
        self.ghost_visible = false;
        self.swallow.set_ghost_visible(false);
        if let Some(id) = self.pending_completion_event.take() {
            self.outcome(id, Outcome::Accepted);
        }
        let text = if whole {
            std::mem::take(&mut self.ghost)
        } else {
            // first word including its leading whitespace and one trailing space if present
            let g = &self.ghost;
            let start = g.len() - g.trim_start().len();
            let rest = &g[start..];
            let end = rest.find(char::is_whitespace).map(|i| start + i + 1).unwrap_or(g.len());
            let end = end.min(g.len());
            let head = g[..end].to_string();
            self.ghost = g[end..].to_string();
            head
        };
        log::debug!("inject {text:?}");
        self.swallow.set_capture(true);
        let result = if use_paste() { cc_inject::paste_text(&text) } else { cc_inject::type_text(&text) };
        self.end_capture();
        if let Err(e) = result {
            log::warn!("inject failed: {e:#}");
        }
        for c in text.chars() {
            self.shadow.push(c);
        }
        let more = !self.ghost.is_empty();
        if more {
            self.show_ghost();
        } else {
            self.cancel_ghost();
            // Tab-Tab chaining: ask for the next sentence once the injected keys have been
            // consumed. UIA calls into the target while it is still processing injected
            // VK_PACKET events garble the text, so hold off for a while first.
            let settle = if use_paste() { Duration::from_millis(50) } else { inject_settle(text.chars().count()) };
            self.last_key = Instant::now() + settle;
            self.pending = true;
        }
    }

    /// Runs after the word before `boundary` is closed. Auto-applies one S0/S1 candidate that
    /// spans exactly that word; anything else is left alone. Never runs after Enter, because
    /// in chat fields the text has already been sent.
    fn on_word_boundary(&mut self, boundary: char) {
        let Some(level) = self.auto_level.clone() else { return };
        let text = self.shadow.text();
        let body = &text[..text.len() - boundary.len_utf8()];
        let start = body.rfind(|c| matches!(c, '.' | '!' | '?' | '\n')).map_or(0, |i| i + 1);
        let sentence = body[start..].trim_start().to_string();
        let word = sentence.rsplit(char::is_whitespace).next().unwrap_or("").to_string();
        if let Some(why) = privacy::sensitive(&sentence) {
            self.log_privacy(why);
            self.prev_boundary = None;
            return;
        }
        self.learn_self_correction(&word);
        if word.is_empty() || self.dont_touch.contains(&word) || cc_correct::is_code_like(&word) {
            self.prev_boundary = Some((word, self.shadow.text().chars().count()));
            return;
        }
        let t0 = Instant::now();
        let n_chars = sentence.chars().count();
        let candidate = match self.auto.lookup(&word) {
            Some(fix) => Some(cc_correct::Candidate {
                start: n_chars - word.chars().count(),
                end: n_chars,
                original: word.clone(),
                replacement: fix,
                rule: "autocorrect".into(),
                severity: Severity::S0,
                confidence: 1.0,
            }),
            None => self
                .checker
                .check(&sentence, &self.dict)
                .into_iter()
                .find(|c| {
                    c.end == n_chars
                        && c.severity <= level
                        && sentence.ends_with(&c.original)
                        && !self.demoted.contains(&c.rule)
                }),
        };
        let Some(c) = candidate else {
            self.prev_boundary = Some((word, self.shadow.text().chars().count()));
            return;
        };
        log::info!(
            "fix {:?} -> {:?} [{} {:?} {:.2}] checked in {} ms",
            c.original, c.replacement, c.rule, c.severity, c.confidence, t0.elapsed().as_millis()
        );
        self.prev_boundary = None;
        self.cancel_ghost();
        let event_id = self
            .store
            .log(&NewEvent {
                app_exe: self.last_exe.clone(),
                source: if c.rule == "autocorrect" { Source::Autocorrect } else { Source::Harper },
                rule: c.rule.clone(),
                severity: format!("{:?}", c.severity),
                confidence: c.confidence,
                context_before: self
                    .policy
                    .store_context
                    .then(|| sentence.chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect()),
                original: c.original.clone(),
                replacement: c.replacement.clone(),
                action: Action::Auto,
                latency_ms: t0.elapsed().as_millis() as u32,
            })
            .unwrap_or_else(|e| {
                log::warn!("store log failed: {e:#}");
                0
            });
        let caret = cc_context::caret_rect();
        let mut typed = c.replacement.clone();
        typed.push(boundary);
        if !self.replace_tail(c.original.chars().count() + 1, &typed) {
            return;
        }
        if let Some(r) = caret {
            let w = (typed.chars().count() as f32 * self.style.font_px * 0.55) as i32;
            self.overlay.show_underline(cc_overlay::Rect { x: r.x - w, y: r.y, w, h: r.h }, UNDERLINE_FADE_MS);
        }
        self.swallow.set_revert_until(cc_hook::now_ms() + REVERT_WINDOW_MS);
        if let Some((id, _, _)) = self.pending_fix_event.replace((event_id, Instant::now(), self.shadow.text().chars().count())) {
            self.outcome(id, Outcome::Kept);
        }
        self.last_fix = Some(Fix { original: c.original, replacement: c.replacement, boundary, event_id });
    }

    /// If the user went back over the previous word and retyped it as a near variant, the
    /// pair becomes an autocorrect entry: that is how the list learns your own typos.
    fn learn_self_correction(&mut self, word: &str) {
        let Some((prev, prev_len)) = self.prev_boundary.take() else { return };
        let now_len = self.shadow.text().chars().count();
        if now_len > prev_len || prev == word || prev.chars().count() < 3 || word.chars().count() < 2 {
            return;
        }
        if cc_correct::is_code_like(&prev) || cc_correct::is_code_like(word) {
            return;
        }
        if cc_correct::edit_distance(&prev.to_lowercase(), &word.to_lowercase()) > 2 {
            return;
        }
        let prev_is_unknown = self.checker.check(&prev, &self.dict).iter().any(|c| c.rule == "spell");
        if !prev_is_unknown {
            return;
        }
        self.auto.add(&prev, word);
        if let Err(e) = self.auto.save(&self.auto_path) {
            log::warn!("autocorrect save failed: {e}");
        }
        log::info!("learned {:?} -> {:?}", prev, word);
    }

    /// Deletes `del` chars before the caret and types `typed`, keeping the shadow in step.
    fn replace_tail(&mut self, del: usize, typed: &str) -> bool {
        let gap = Duration::from_millis(fix_key_gap_ms());
        // Capture the user's keys for the duration, and let the boundary key that triggered
        // us settle in the target before deleting behind it.
        self.swallow.set_capture(true);
        std::thread::sleep(Duration::from_millis(fix_pre_delay_ms()));
        let result = match fix_inject_mode().as_str() {
            "fast" => cc_inject::backspace(del).and_then(|_| cc_inject::type_text(typed)),
            "paste" => cc_inject::backspace_slow(del, gap).and_then(|_| cc_inject::paste_text(typed)),
            "keys-burst" => cc_inject::backspace(del).and_then(|_| cc_inject::type_text_keys(typed, Duration::ZERO)),
            "keys-slow" => cc_inject::backspace_slow(del, gap).and_then(|_| cc_inject::type_text_keys(typed, gap)),
            "slow" => cc_inject::backspace_slow(del, gap).and_then(|_| cc_inject::type_text_slow(typed, gap)),
            // Default: Notepad drops some of a burst of identical Backspace presses, so space
            // them out; the replacement itself can go in one burst of real key events.
            _ => cc_inject::backspace_slow(del, gap).and_then(|_| cc_inject::type_text_keys(typed, Duration::ZERO)),
        };
        self.end_capture();
        if let Err(e) = result {
            log::warn!("correction inject failed: {e:#}");
            self.resync();
            return false;
        }
        for _ in 0..del {
            self.shadow.backspace();
        }
        for ch in typed.chars() {
            self.shadow.push(ch);
        }
        self.last_key = Instant::now() + inject_settle(del + typed.chars().count());
        self.pending = true;
        true
    }

    /// Stops capturing; captured keys are replayed on a later tick, once the hook thread has
    /// delivered all of them.
    fn end_capture(&mut self) {
        self.swallow.set_capture(false);
        self.replay_at = Some(Instant::now() + Duration::from_millis(30));
    }

    fn flush_replay(&mut self) {
        if self.replay.is_empty() {
            return;
        }
        let keys = std::mem::take(&mut self.replay);
        log::debug!("replay {keys:?}");
        for c in keys.chars() {
            let ev = match c {
                '\u{8}' => {
                    let _ = cc_inject::backspace(1);
                    HookEvent::Backspace
                }
                '\n' => {
                    let _ = cc_inject::type_text_keys("\n", Duration::ZERO);
                    HookEvent::Enter
                }
                _ => {
                    let _ = cc_inject::type_text_keys(&c.to_string(), Duration::ZERO);
                    HookEvent::Char(c)
                }
            };
            self.on_key(ev);
        }
    }

    fn revert_fix(&mut self) {
        self.swallow.set_revert_until(0);
        let Some(fix) = self.last_fix.take() else { return };
        self.cancel_ghost();
        let mut typed = fix.original.clone();
        typed.push(fix.boundary);
        if !self.replace_tail(fix.replacement.chars().count() + 1, &typed) {
            return;
        }
        let n = self.reverts.entry(fix.original.clone()).or_insert(0);
        *n += 1;
        if *n >= 2 {
            self.dict.add(&fix.original);
            if let Err(e) = self.dict.save(&self.dict_path) {
                log::warn!("dictionary save failed: {e}");
            }
            log::info!("added {:?} to the personal dictionary", fix.original);
        }
        self.pending_fix_event = None;
        self.outcome(fix.event_id, Outcome::Reverted);
        self.dont_touch.insert(fix.original.clone());
        self.auto.remove(&fix.original);
        if let Err(e) = self.auto.save(&self.auto_path) {
            log::warn!("autocorrect save failed: {e}");
        }
        log::info!("reverted {:?} -> {:?}", fix.replacement, fix.original);
    }

    fn on_key(&mut self, ev: HookEvent) {
        log::trace!("key {ev:?} ghost_visible={} ghost={:?}", self.ghost_visible, self.ghost);
        if !self.active() {
            self.cancel_ghost();
            return;
        }
        self.last_key = Instant::now();
        let modifier_only = matches!(ev, HookEvent::Other(vk) if is_modifier_vk(vk));
        if self.last_fix.is_some() && !modifier_only && ev != HookEvent::RevertRequested {
            self.last_fix = None;
            self.swallow.set_revert_until(0);
        }
        match ev {
            HookEvent::Char(c) => {
                self.shadow.push(c);
                if c.is_whitespace() || c.is_ascii_punctuation() {
                    self.on_word_boundary(c);
                    if self.last_fix.is_some() {
                        return;
                    }
                }
                if self.ghost_visible && self.ghost.starts_with(c) {
                    self.ghost.remove(0);
                    if self.ghost.trim().is_empty() {
                        self.cancel_ghost();
                        self.pending = true;
                    } else {
                        self.show_ghost();
                    }
                } else {
                    self.cancel_ghost();
                    self.pending = true;
                }
            }
            HookEvent::Backspace => {
                self.shadow.backspace();
                if let Some((id, _, len)) = self.pending_fix_event {
                    if self.shadow.text().chars().count() < len {
                        self.pending_fix_event = None;
                        self.outcome(id, Outcome::Overwritten);
                    }
                }
                self.cancel_ghost();
                self.pending = true;
            }
            HookEvent::Enter => {
                self.shadow.push('\n');
                self.cancel_ghost();
                self.pending = true;
            }
            HookEvent::Tab => self.accept(true),
            HookEvent::CtrlTab => self.accept(false),
            HookEvent::Esc => self.cancel_ghost(),
            HookEvent::Other(vk) if is_modifier_vk(vk) => {}
            HookEvent::Left | HookEvent::Right | HookEvent::Nav | HookEvent::Other(_) => self.resync(),
            HookEvent::RevertRequested => self.revert_fix(),
            HookEvent::Captured(c) => self.replay.push(c),
        }
    }

    fn on_token(&mut self, gen: u64, text: String, logprob: Option<f32>) {
        if gen != self.gen {
            return;
        }
        if let Some(lp) = logprob {
            self.logprob_sum += lp;
        }
        self.tok_count += 1;
        if !self.first_token_logged {
            self.first_token_logged = true;
            self.completion_latency_ms = self.last_key.elapsed().as_millis() as u32;
            if let Some(t0) = self.req_started {
                log::info!(
                    "latency: keystroke->first token {} ms (request->first {} ms)",
                    self.completion_latency_ms,
                    t0.elapsed().as_millis()
                );
            }
        }
        // A newline in a ghost would submit the line in chat fields and terminals.
        self.raw.push_str(&text.replace(['\r', '\n'], ""));
        if degenerate(&self.raw) {
            self.cancel_ghost();
            return;
        }
        if self.need_space && !self.raw.is_empty() && !self.raw.starts_with(char::is_whitespace) {
            log::debug!("continuation glued to the last word, dropped: {:?}", self.raw);
            self.cancel_ghost();
            return;
        }
        // Healing: the raw text must reproduce the half-typed word before the ghost begins.
        let candidate = self.raw.trim_start();
        let ghost = if self.heal.is_empty() {
            if self.need_space { candidate.to_string() } else { self.raw.clone() }
        } else if candidate.len() < self.heal.len() {
            if self.heal.to_lowercase().starts_with(&candidate.to_lowercase()) {
                return; // still consistent, wait for more
            }
            log::debug!("healing mismatch {:?} vs {:?}", candidate, self.heal);
            self.cancel_ghost();
            return;
        } else if candidate.to_lowercase().starts_with(&self.heal.to_lowercase()) {
            candidate[self.heal.len()..].to_string()
        } else {
            log::debug!("healing mismatch {:?} vs {:?}", candidate, self.heal);
            self.cancel_ghost();
            return;
        };
        if ghost.trim().is_empty() {
            return;
        }
        // Confidence gate on the running mean token probability, judged after a few tokens.
        if self.tok_count >= 3 {
            let conf = (self.logprob_sum / self.tok_count as f32).exp();
            if conf < self.min_conf {
                log::debug!("low confidence {conf:.2} < {:.2}, hidden: {:?}", self.min_conf, self.raw);
                self.cancel_ghost();
                return;
            }
        } else if !self.ghost_visible {
            self.ghost = ghost;
            return; // not enough evidence yet; keep accumulating silently
        }
        self.ghost = ghost;
        self.show_ghost();
    }

    fn on_done(&mut self, gen: u64, result: anyhow::Result<Completion>) {
        if gen != self.gen {
            return;
        }
        match result {
            Ok(c) => {
                log::info!(
                    "completion: {:?} logprob={:?} prompt_ms={:.0} predicted_ms={:.0}",
                    c.text,
                    c.mean_logprob,
                    c.prompt_ms,
                    c.predicted_ms
                );
                let conf = c.mean_logprob.map_or(1.0, f32::exp);
                if c.text.trim().is_empty() || degenerate(&c.text) || conf < self.min_conf {
                    self.cancel_ghost();
                } else if self.ghost.trim().is_empty() {
                    self.cancel_ghost();
                } else {
                    if !self.ghost_visible {
                        // Short completions never reached the 3-token gate; show them now.
                        self.show_ghost();
                    }
                    let ev = NewEvent {
                        app_exe: self.last_exe.clone(),
                        source: Source::LlmComplete,
                        rule: "complete".into(),
                        severity: "S4".into(),
                        confidence: c.mean_logprob.map_or(0.0, |lp| lp.exp()),
                        context_before: None,
                        original: String::new(),
                        replacement: c.text.clone(),
                        action: Action::Suggested,
                        latency_ms: self.completion_latency_ms,
                    };
                    self.last_prompt_ms = c.prompt_ms;
                    self.record_context(&c.text, conf, None);
                    match self.store.log(&ev) {
                        Ok(id) => {
                            self.pending_completion_event = Some(id);
                            self.last_context_event = Some(id);
                        }
                        Err(e) => log::warn!("store log failed: {e:#}"),
                    }
                }
            }
            Err(e) => {
                if !self.cancel.is_cancelled() {
                    log::warn!("completion error: {e:#}");
                }
                self.cancel_ghost();
            }
        }
    }

    fn on_tray(&mut self, ev: TrayEvent) {
        log::info!("tray: {ev:?}");
        match ev {
            TrayEvent::Enabled(b) => {
                self.enabled = b;
                self.paused_until = None;
                if !b {
                    self.cancel_ghost();
                }
                self.save_settings();
            }
            TrayEvent::PauseMinutes(m) => {
                self.paused_until = Some(Instant::now() + Duration::from_secs(u64::from(m) * 60));
                self.cancel_ghost();
            }
            TrayEvent::AutoFixLevel(s) => {
                self.auto_level = parse_level(&s);
                self.save_settings();
            }
            TrayEvent::OpenProfileFolder => {
                let _ = std::process::Command::new("explorer.exe").arg(&self.profile).spawn();
            }
            TrayEvent::ShowMetrics => self.refresh_status(true),
            TrayEvent::Model(id) => self.switch_model(&id),
            TrayEvent::IconClick => self.windows.toggle_flyout(&self.ui.url("flyout")),
            TrayEvent::OpenDashboard => self.windows.open_dashboard(&self.ui.url("dashboard")),
            TrayEvent::Quit => {
                log::info!("quit from tray");
                std::process::exit(0);
            }
        }
        self.tray.set_state(&TrayState {
            enabled: self.enabled,
            auto_fix_level: level_name(&self.auto_level),
            model: self.model_id.clone(),
            status_line: String::new(),
        });
    }

    /// Downloads the model if needed and restarts the engine on it. On failure the
    /// previous model is brought back.
    fn switch_model(&mut self, id: &str) {
        if id == self.model_id || cc_engine::model_spec(id).is_none() {
            return;
        }
        self.cancel_ghost();
        let previous = self.model_id.clone();
        self.sidecar = None;
        match self.start_engine(id) {
            Ok(()) => {
                self.model_id = id.to_string();
                self.save_settings();
                self.refresh_status(true);
            }
            Err(e) => {
                log::warn!("switching to {id} failed: {e:#}; back to {previous}");
                self.tray.set_status(&format!("Model switch failed: {e}"));
                if let Err(e) = self.start_engine(&previous) {
                    log::error!("restarting {previous} failed: {e:#}");
                }
            }
        }
    }

    fn start_engine(&mut self, id: &str) -> anyhow::Result<()> {
        let root = self.root.clone();
        let tray = &self.tray;
        let label = cc_engine::model_spec(id).map_or(id, |m| m.label);
        tray.set_status(&format!("Loading {label}"));
        let sidecar = self.rt.block_on(async {
            let mut last = Instant::now() - Duration::from_secs(5);
            let assets = cc_engine::ensure_assets_for(&root, id, |what, done, total| {
                if last.elapsed() >= Duration::from_secs(2) {
                    last = Instant::now();
                    let line = match total {
                        Some(t) => format!("{what}: {} / {} MB", done >> 20, t >> 20),
                        None => format!("{what}: {} MB", done >> 20),
                    };
                    log::info!("{line}");
                    tray.set_status(&line);
                }
            })
            .await
            .context("ensure assets")?;
            Sidecar::start(&assets, PORT).await.context("start llama-server")
        })?;
        log::info!("engine ready: {} ({label})", sidecar.base_url());
        self.sidecar = Some(sidecar);
        Ok(())
    }

    fn refresh_status(&mut self, log_it: bool) {
        self.last_status = Instant::now();
        let day_ago = unix_ms() - 24 * 3600 * 1000;
        match self.store.metrics(Some(day_ago)) {
            Ok(m) => {
                let state = if !self.enabled {
                    "Off"
                } else if self.paused_until.is_some_and(|t| Instant::now() < t) {
                    "Paused"
                } else if self.sidecar.is_none() {
                    "Completions off (model not loaded)"
                } else {
                    "Ready"
                };
                let line = format!(
                    "{state} · 24h: {} fixes kept, {} reverted, {} sentences accepted, {} chars saved, p50 {} ms",
                    m.fixes_auto.saturating_sub(m.fixes_reverted), m.fixes_reverted, m.completions_accepted, m.chars_saved, m.p50_latency_ms
                );
                if log_it {
                    log::info!("{line}");
                }
                self.tray.set_status(&line);
                if let Ok(mut s) = self.ui_state.settings.lock() {
                    s.status_line = line;
                }
            }
            Err(e) => log::warn!("metrics failed: {e:#}"),
        }
    }

    fn run_tuning(&mut self) {
        self.last_tune = Instant::now();
        match self.store.expire_pending(24 * 3600 * 1000) {
            Ok(n) if n > 0 => log::info!("expired {n} pending events"),
            Err(e) => log::warn!("expire failed: {e:#}"),
            _ => {}
        }
        match self.store.tune() {
            Ok(changes) => {
                for c in &changes {
                    log::info!("tuning: {c:?}");
                    if let cc_store::Tuning::Demote { rule, .. } = c {
                        self.demoted.insert(rule.clone());
                    }
                }
            }
            Err(e) => log::warn!("tune failed: {e:#}"),
        }
    }

    fn refresh_style(&mut self) {
        self.style_refreshed = Instant::now();
        let app = if self.last_exe.is_empty() { None } else { Some(self.last_exe.as_str()) };
        match self.store.recent_accepted(6, app) {
            Ok(lines) => {
                let lines: Vec<String> = lines.into_iter().map(|l| l.trim().to_string()).filter(|l| l.len() > 10).collect();
                if lines != self.style_lines {
                    log::debug!("style prefix: {} lines", lines.len());
                    self.style_lines = lines;
                }
            }
            Err(e) => log::warn!("recent_accepted failed: {e:#}"),
        }
    }

    fn tick(&mut self) {
        while let Ok(ev) = self.tray.events().try_recv() {
            self.on_tray(ev);
        }
        while let Ok(cmd) = self.ui_cmds.try_recv() {
            self.on_ui_command(cmd);
        }
        if let Some((id, at, _)) = self.pending_fix_event {
            if at.elapsed() >= KEPT_AFTER {
                self.pending_fix_event = None;
                self.outcome(id, Outcome::Kept);
            }
        }
        if self.last_status.elapsed() >= STATUS_EVERY {
            self.refresh_status(false);
        }
        if self.last_tune.elapsed() >= TUNE_EVERY {
            self.run_tuning();
        }
        if self.style_refreshed.elapsed() >= STYLE_EVERY {
            self.refresh_style();
        }
        if let Some(at) = self.replay_at {
            if Instant::now() >= at {
                self.replay_at = None;
                self.flush_replay();
            }
        }
        if self.pending && self.last_key.elapsed() >= DEBOUNCE {
            self.request_completion();
        }
    }
}

fn run(rx: Receiver<Msg>, mut app: App) {
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Msg::Key(ev)) => app.on_key(ev),
            Ok(Msg::Token { gen, text, logprob }) => app.on_token(gen, text, logprob),
            Ok(Msg::Done { gen, result }) => app.on_done(gen, result),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        app.tick();
    }
}

/// Runaway output such as `100000000000` or `aaaaaaa` is never worth showing.
fn degenerate(text: &str) -> bool {
    let mut run = 0;
    let mut prev = None;
    for c in text.chars() {
        if Some(c) == prev {
            run += 1;
            if run >= 5 && !c.is_whitespace() {
                return true;
            }
        } else {
            run = 0;
            prev = Some(c);
        }
    }
    false
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn data_root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("CaseComplete")
}

/// A windowed app has no stderr, so an error returned from `main` would vanish without a
/// trace: it is logged and shown in a message box instead.
fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default_hook(info);
    }));
    if let Err(e) = start() {
        log::error!("fatal: {e:#}");
        message_box(
            &format!("CaseComplete could not start.\n\n{e:#}\n\nLog: {}", log_path().display()),
            MB_ICONERROR,
        );
        std::process::exit(1);
    }
}

fn log_path() -> PathBuf {
    data_root().join("casecomplete.log")
}

fn message_box(text: &str, icon: MESSAGEBOX_STYLE) {
    // SAFETY: plain Win32 call with valid strings and no owner window.
    unsafe {
        MessageBoxW(HWND::default(), &HSTRING::from(text), w!("CaseComplete"), MB_OK | icon);
    }
}

fn start() -> anyhow::Result<()> {
    let root = data_root();
    std::fs::create_dir_all(&root)?;
    // A windowed app: no console unless CC_CONSOLE=1 asks for one; otherwise the log
    // goes to %LOCALAPPDATA%\CaseComplete\casecomplete.log (truncated past 5 MB).
    let mut logger = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if std::env::var_os("CC_CONSOLE").is_some() {
        // SAFETY: plain Win32 call; failure only means a console already exists.
        unsafe {
            let _ = windows::Win32::System::Console::AllocConsole();
        }
    } else {
        let path = log_path();
        if std::fs::metadata(&path).map_or(false, |m| m.len() > 5 << 20) {
            let _ = std::fs::remove_file(&path);
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&path).context("open log file")?;
        logger.target(env_logger::Target::Pipe(Box::new(log)));
    }
    logger.init();

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;

    let (tx, rx) = unbounded::<Msg>();
    let client = CompletionClient::new(&sidecar_url());
    let engine = spawn_engine(client, tx.clone());

    let overlay = Overlay::new().context("overlay")?;
    let swallow = Arc::new(SwallowState::default());

    let (ktx, krx) = unbounded::<HookEvent>();
    let _hook = cc_hook::start(ktx, swallow.clone()).context("keyboard hook")?;
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("cc-keyfwd".into())
            .spawn(move || {
                for ev in krx {
                    if tx.send(Msg::Key(ev)).is_err() {
                        break;
                    }
                }
            })?;
    }

    cc_context::init_thread().context("UIA init")?;
    let profile = root.join("profiles").join("default");
    std::fs::create_dir_all(&profile)?;
    let dict_path = profile.join("dictionary.txt");
    let auto_path = profile.join("autocorrect.txt");
    let settings: Settings = std::fs::read_to_string(profile.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let auto_level = match std::env::var("CC_AUTO_APPLY_UP_TO") {
        Ok(s) => parse_level(&s),
        Err(_) => parse_level(&settings.auto_fix_level),
    };
    let store = Store::open(&profile.join("events.db")).context("open events.db")?;
    let model_id = match std::env::var("CC_MODEL") {
        Ok(m) if cc_engine::model_spec(&m).is_some() => m,
        _ if cc_engine::model_spec(&settings.model).is_some() => settings.model.clone(),
        _ => DEFAULT_MODEL.to_string(),
    };
    let tray = Tray::new(TrayState {
        enabled: settings.enabled,
        auto_fix_level: level_name(&auto_level),
        model: model_id.clone(),
        status_line: "Starting".into(),
    })
    .context("tray icon")?;
    let policy = Policy::load(&profile.join("privacy.toml"));
    let ui_state = Arc::new(UiState::default());
    let (ui_tx, ui_cmds) = unbounded::<UiCommand>();
    let ui = UiServer::start(&profile.join("events.db"), &profile, ui_state.clone(), ui_tx).context("ui server")?;
    let windows = Windows::new().context("ui windows")?;
    log::info!("dashboard: {}", ui.url("dashboard"));
    let long_ago = Instant::now() - Duration::from_secs(24 * 3600);
    let mut app = App {
        store,
        tray,
        profile: profile.clone(),
        root: root.clone(),
        policy,
        model_id: model_id.clone(),
        rt,
        sidecar: None,
        denied_hwnd: 0,
        ui_state,
        ui,
        ui_cmds,
        windows,
        last_prompt_tail: String::new(),
        enabled: settings.enabled,
        paused_until: None,
        demoted: HashSet::new(),
        pending_fix_event: None,
        pending_completion_event: None,
        completion_latency_ms: 0,
        raw: String::new(),
        heal: String::new(),
        need_space: false,
        last_prompt_ms: 0.0,
        last_context_event: None,
        tok_count: 0,
        logprob_sum: 0.0,
        min_conf: std::env::var("CC_MIN_CONFIDENCE").ok().and_then(|v| v.parse().ok()).unwrap_or(settings.min_confidence),
        style_lines: Vec::new(),
        style_refreshed: long_ago,
        last_tune: long_ago,
        last_status: long_ago,
        checker: Checker::new(),
        dict: PersonalDictionary::load(&dict_path),
        auto: AutocorrectList::load(&auto_path),
        auto_path,
        prev_boundary: None,
        dict_path,
        auto_level,
        last_fix: None,
        replay: String::new(),
        replay_at: None,
        dont_touch: HashSet::new(),
        reverts: HashMap::new(),
        shadow: ShadowBuffer::new(),
        ghost: String::new(),
        ghost_visible: false,
        gen: 0,
        cancel: CancellationToken::new(),
        pending: false,
        last_key: Instant::now(),
        req_started: None,
        first_token_logged: false,
        swallow,
        overlay,
        engine,
        style: GhostStyle::default(),
        last_exe: String::new(),
        last_hwnd: 0,
        in_terminal: false,
    };
    if let Err(e) = app.start_engine(&model_id) {
        // Corrections need no model, so a failed download (blocked network, a proxy
        // certificate we don't trust) turns completion off instead of ending the app.
        log::error!("engine failed to start: {e:#}");
        let text = format!(
            "CaseComplete is running in the system tray, but completions are off because the model could not be loaded.\n\n{e:#}\n\nCorrections still work. Restart CaseComplete to try again.\n\nLog: {}",
            log_path().display()
        );
        // On its own thread: a modal box here would stall the orchestrator.
        std::thread::spawn(move || message_box(&text, MB_ICONWARNING));
    }
    app.run_tuning();
    app.push_ui_settings();
    app.refresh_status(true);
    log::info!(
        "casecomplete running. Tab accepts, Ctrl+Tab one word, Esc dismisses; auto-fix up to {:?}, Left/Backspace within 3 s reverts.",
        app.auto_level
    );
    run(rx, app);
    Ok(())
}

/// CC_FIX_INJECT=slow (default) | paste | fast: how a correction is written into the field.
fn fix_inject_mode() -> String {
    static MODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MODE.get_or_init(|| std::env::var("CC_FIX_INJECT").unwrap_or_else(|_| "keys".into())).clone()
}

/// Wait before deleting behind a just-typed boundary key (CC_FIX_PRE_DELAY_MS, default 40).
fn fix_pre_delay_ms() -> u64 {
    static D: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var("CC_FIX_PRE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(40))
}

/// Gap between injected keys in slow mode (CC_FIX_KEY_GAP_MS, default 4).
fn fix_key_gap_ms() -> u64 {
    static GAP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *GAP.get_or_init(|| std::env::var("CC_FIX_KEY_GAP_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(12))
}

/// CC_INJECT=type uses KEYEVENTF_UNICODE SendInput; the default pastes via the clipboard,
/// which Windows 11 Notepad does not garble.
fn use_paste() -> bool {
    static PASTE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PASTE.get_or_init(|| std::env::var("CC_INJECT").map_or(true, |v| v != "type"))
}

/// How long the target app may take to drain `chars` injected key events before we touch
/// it through UIA again. Overridable with CC_SETTLE_MS_PER_CHAR for experiments.
fn inject_settle(chars: usize) -> Duration {
    static PER_CHAR: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let per = *PER_CHAR.get_or_init(|| {
        std::env::var("CC_SETTLE_MS_PER_CHAR").ok().and_then(|v| v.parse().ok()).unwrap_or(8)
    });
    Duration::from_millis((chars as u64 * per).max(100))
}

/// Shift/Ctrl/Alt/Win/CapsLock presses carry no text and must not trigger a resync.
fn is_modifier_vk(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0x5B | 0x5C | 0xA0..=0xA5)
}

fn sidecar_url() -> String {
    format!("http://127.0.0.1:{PORT}")
}
