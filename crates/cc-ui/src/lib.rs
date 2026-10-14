//! Local web UI for CaseComplete: an HTTP server on `127.0.0.1` that serves one embedded
//! single-page app (tray flyout and dashboard) and a small JSON API over the event log
//! (`cc-store`), the profile files and the settings shared with the orchestrator.
//!
//! The server never writes anything itself. Settings and word-list edits become
//! [`UiCommand`]s on the channel handed to [`UiServer::start`]; the orchestrator applies
//! them, persists them and keeps [`UiState`] current. Reads go through short-lived
//! read-only SQLite connections, so a busy writer never blocks a page.
//!
//! Access control: a random token is generated per run and put in [`UiServer::url`].
//! `GET /` needs it as `?t=`; the page keeps it in `localStorage` and sends it back as the
//! `X-Token` header on every `/api` call. Requests are also refused when their `Host` or
//! `Origin` is not this server, and no CORS headers are ever sent, so neither other local
//! programs nor other web pages can read the data.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context};
use crossbeam_channel::Sender;

mod api;
mod auth;
mod http;
mod profile;
mod router;
#[cfg(test)]
mod tests;

/// One model the user can pick.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    pub origin: String,
    pub installed: bool,
}

/// Settings as the orchestrator currently has them (what `/api/settings` shows).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct UiSettings {
    pub enabled: bool,
    pub paused_until_ms: Option<i64>,
    /// `"off"`, `"S0"`, `"S1"` or `"S2"`.
    pub auto_fix_level: String,
    pub model: String,
    pub min_confidence: f32,
    pub models: Vec<ModelInfo>,
    pub profile_dir: String,
    pub autostart: bool,
    pub status_line: String,
}

/// What the model saw for the most recent suggestion (the dashboard's Context page).
#[derive(Clone, Debug, serde::Serialize, Default)]
pub struct ContextSnapshot {
    pub ts_ms: i64,
    pub app_exe: String,
    pub style_lines: Vec<String>,
    pub prompt_tail: String,
    pub heal: String,
    pub need_space: bool,
    pub completion: String,
    pub confidence: f32,
    pub tokens: u32,
    pub latency_ms: u32,
    pub prompt_ms: f32,
    pub outcome: Option<String>,
    pub privacy_block: Option<String>,
}

/// A change the UI asks the orchestrator to make.
#[derive(Clone, Debug, PartialEq)]
pub enum UiCommand {
    SetEnabled(bool),
    PauseMinutes(u32),
    SetAutoFixLevel(String),
    SetModel(String),
    SetMinConfidence(f32),
    AddDictionaryWord(String),
    RemoveDictionaryWord(String),
    /// `(typo, fix)`
    SetAutocorrect(String, String),
    RemoveAutocorrect(String),
    SetAutostart(bool),
    OpenProfileFolder,
    Quit,
}

/// State the orchestrator publishes and the server reads (and updates optimistically
/// after a settings change, so the page reflects a click before the orchestrator answers).
#[derive(Default)]
pub struct UiState {
    pub settings: Mutex<UiSettings>,
    pub last_context: Mutex<Option<ContextSnapshot>>,
}

/// Lock, ignoring poison: a panic elsewhere must not take the UI down with it.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Everything a request handler needs.
pub(crate) struct Ctx {
    pub db: PathBuf,
    pub profile: PathBuf,
    pub state: Arc<UiState>,
    pub commands: Sender<UiCommand>,
    pub token: String,
    pub port: u16,
}

const WORKERS: usize = 4;
const POLL: Duration = Duration::from_millis(100);

/// The running server. It stops when dropped.
pub struct UiServer {
    port: u16,
    token: String,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl UiServer {
    /// Binds 127.0.0.1 on a free port, serves until dropped. `db` is the events.db path
    /// (opened read-only per request, WAL allows it); `profile` holds dictionary.txt,
    /// autocorrect.txt, privacy.toml and (optionally) privacy_log.jsonl.
    pub fn start(
        db: &Path,
        profile: &Path,
        state: Arc<UiState>,
        commands: Sender<UiCommand>,
    ) -> anyhow::Result<UiServer> {
        // A read-only connection cannot create the file; make sure a fresh profile has one.
        if !db.exists() {
            cc_store::Store::open(db)
                .with_context(|| format!("create event log {}", db.display()))?;
        }
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|e| anyhow!("cannot bind 127.0.0.1: {e}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .map(|a| a.port())
            .ok_or_else(|| anyhow!("UI server is not listening on an IP socket"))?;
        let token = auth::new_token()?;
        let ctx = Arc::new(Ctx {
            db: db.to_path_buf(),
            profile: profile.to_path_buf(),
            state,
            commands,
            token: token.clone(),
            port,
        });
        let server = Arc::new(server);
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(WORKERS);
        for i in 0..WORKERS {
            let (server, ctx, stop) = (server.clone(), ctx.clone(), stop.clone());
            let handle = std::thread::Builder::new()
                .name(format!("cc-ui-{i}"))
                .spawn(move || worker(&server, &ctx, &stop))
                .context("spawn UI worker")?;
            workers.push(handle);
        }
        log::info!("ui: listening on 127.0.0.1:{port}");
        Ok(UiServer { port, token, stop, workers })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Address of a page, token included, e.g. `url("dashboard")` is
    /// `http://127.0.0.1:PORT/?t=TOKEN#/dashboard` and `url("flyout")` ends in `#/flyout`.
    pub fn url(&self, view: &str) -> String {
        let view: String = view
            .trim_start_matches(['#', '/'])
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/'))
            .collect();
        format!("http://127.0.0.1:{}/?t={}#/{}", self.port, self.token, view)
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn worker(server: &tiny_http::Server, ctx: &Ctx, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        match server.recv_timeout(POLL) {
            Ok(Some(request)) => http::serve(ctx, request),
            Ok(None) => {}
            Err(e) => {
                log::warn!("ui: accept failed: {e}");
                std::thread::sleep(POLL);
            }
        }
    }
}
