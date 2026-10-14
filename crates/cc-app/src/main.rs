//! CaseComplete orchestrator (M0 spike). Wires hook -> context -> engine -> overlay -> inject.
//!
//! One orchestrator thread owns all state and receives every event on one channel:
//! keys from the hook thread, streamed tokens from the engine runtime, and a periodic tick.
//! The hook thread never does work; the engine runs on its own tokio runtime; UIA calls
//! happen only on the orchestrator thread (which calls `cc_context::init_thread`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use cc_engine::{Completion, CompletionClient};
use cc_hook::{HookEvent, ShadowBuffer, SwallowState};
use cc_overlay::{GhostStyle, Overlay};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use tokio_util::sync::CancellationToken;

const PORT: u16 = 8089;
const DEBOUNCE: Duration = Duration::from_millis(100);
const N_PREDICT: u32 = 24;
const CONTEXT_CHARS: usize = 800;
const EXCLUDED_EXES: &[&str] = &[
    "windowsterminal.exe",
    "1password.exe",
    "keepassxc.exe",
    "casecomplete.exe",
    "conhost.exe",
];

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

struct App {
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
                !fi.is_password && fi.is_editable && !EXCLUDED_EXES.contains(&fi.exe.as_str())
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
        if let Some(t) = cc_context::text_before_caret(CONTEXT_CHARS) {
            if t.len() >= self.shadow.text().len() {
                self.shadow.reset(&t);
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
        let result = if use_paste() { cc_inject::paste_text(&text) } else { cc_inject::type_text(&text) };
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

    fn on_key(&mut self, ev: HookEvent) {
        log::trace!("key {ev:?} ghost_visible={} ghost={:?}", self.ghost_visible, self.ghost);
        self.last_key = Instant::now();
        match ev {
            HookEvent::Char(c) => {
                self.shadow.push(c);
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
            HookEvent::RevertRequested => {
                // M2: revert last auto-applied correction. Nothing auto-applied in M0.
            }
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
    let app = App {
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
    };
    log::info!("casecomplete M0 running. Type in any editable field; Tab accepts, Ctrl+Tab one word, Esc dismisses.");
    run(rx, app);
    Ok(())
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
