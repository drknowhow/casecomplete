//! CaseComplete orchestrator (M0 spike). Wires hook -> context -> engine -> overlay -> inject.
//!
//! One orchestrator thread owns all state and receives every event on one channel:
//! keys from the hook thread, streamed tokens from the engine runtime, and a periodic tick.
//! The hook thread never does work; the engine runs on its own tokio runtime; UIA calls
//! happen only on the orchestrator thread (which calls `cc_context::init_thread`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

use anyhow::Context;
use cc_correct::{AutocorrectList, Checker, PersonalDictionary, Severity};
use cc_engine::{Completion, CompletionClient};
use cc_hook::{HookEvent, ShadowBuffer, SwallowState};
use cc_overlay::{GhostStyle, Overlay};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use tokio_util::sync::CancellationToken;

const PORT: u16 = 8089;
const DEBOUNCE: Duration = Duration::from_millis(100);
const N_PREDICT: u32 = 24;
const CONTEXT_CHARS: usize = 800;
const REVERT_WINDOW_MS: u64 = 3000;
const UNDERLINE_FADE_MS: u32 = 1500;
const EXCLUDED_EXES: &[&str] = &["1password.exe", "keepassxc.exe", "casecomplete.exe"];
/// Terminals: UIA reports the whole screen buffer, not an edit field, so the shadow buffer
/// is the only context and editability is assumed.
const TERMINAL_EXES: &[&str] = &["windowsterminal.exe", "conhost.exe", "openconsole.exe", "wezterm-gui.exe", "alacritty.exe"];

enum Msg {
    Key(HookEvent),
    Token { gen: u64, text: String },
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
                            .complete(&req.prompt, N_PREDICT, req.cancel, move |tok| {
                                let _ = out_tok.send(Msg::Token { gen, text: tok.to_string() });
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
    fn cancel_ghost(&mut self) {
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
                if fi.hwnd != self.last_hwnd {
                    log::info!("focus: {} editable={} password={}", fi.exe, fi.is_editable, fi.is_password);
                    self.last_exe = fi.exe.clone();
                    self.last_hwnd = fi.hwnd;
                    // A different window means the shadow buffer describes the wrong field.
                    self.shadow.reset("");
                }
                self.in_terminal = TERMINAL_EXES.contains(&fi.exe.as_str());
                !fi.is_password
                    && (fi.is_editable || self.in_terminal)
                    && !EXCLUDED_EXES.contains(&fi.exe.as_str())
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
        self.cancel_ghost();
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
                .find(|c| c.end == n_chars && c.severity <= level && sentence.ends_with(&c.original)),
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
        self.last_fix = Some(Fix { original: c.original, replacement: c.replacement, boundary });
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
        self.dont_touch.insert(fix.original.clone());
        self.auto.remove(&fix.original);
        if let Err(e) = self.auto.save(&self.auto_path) {
            log::warn!("autocorrect save failed: {e}");
        }
        log::info!("reverted {:?} -> {:?}", fix.replacement, fix.original);
    }

    fn on_key(&mut self, ev: HookEvent) {
        log::trace!("key {ev:?} ghost_visible={} ghost={:?}", self.ghost_visible, self.ghost);
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

    fn on_token(&mut self, gen: u64, text: String) {
        if gen != self.gen {
            return;
        }
        if !self.first_token_logged {
            self.first_token_logged = true;
            if let Some(t0) = self.req_started {
                log::info!(
                    "latency: keystroke->first token {} ms (request->first {} ms)",
                    self.last_key.elapsed().as_millis(),
                    t0.elapsed().as_millis()
                );
            }
        }
        self.ghost.push_str(&text);
        if self.ghost.trim().is_empty() {
            return;
        }
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
                if c.text.trim().is_empty() {
                    self.cancel_ghost();
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

    fn tick(&mut self) {
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
            Ok(Msg::Token { gen, text }) => app.on_token(gen, text),
            Ok(Msg::Done { gen, result }) => app.on_done(gen, result),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        app.tick();
    }
}

fn data_root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("CaseComplete")
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let root = data_root();
    std::fs::create_dir_all(&root)?;

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let (assets, sidecar) = rt.block_on(async {
        let mut last = Instant::now();
        let assets = cc_engine::ensure_assets(&root, |label, done, total| {
            if last.elapsed() > Duration::from_secs(2) {
                last = Instant::now();
                match total {
                    Some(t) => log::info!("{label}: {} / {} MB", done >> 20, t >> 20),
                    None => log::info!("{label}: {} MB", done >> 20),
                }
            }
        })
        .await
        .context("ensure assets")?;
        let sidecar = cc_engine::Sidecar::start(&assets, PORT).await.context("start llama-server")?;
        anyhow::Ok((assets, sidecar))
    })?;
    log::info!("engine ready: {} ({})", sidecar.base_url(), assets.model.display());
    // keep the sidecar alive for the lifetime of the app
    let _sidecar = sidecar;

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
    let auto_level = match std::env::var("CC_AUTO_APPLY_UP_TO").as_deref() {
        Ok("off") => None,
        Ok("S0") => Some(Severity::S0),
        Ok("S2") => Some(Severity::S2),
        _ => Some(Severity::S1),
    };
    let app = App {
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
