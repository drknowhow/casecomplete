//! Completion engine: manages the bundled `llama-server` sidecar and streams completions.
//!
//! Contract:
//! - `ensure_assets(root)`: make sure `root/bin/llama-server.exe` (+ DLLs) and
//!   `root/models/<default>.gguf` exist; download with progress + sha256 check if not.
//!   Prefer the CUDA build of a recent ggml-org/llama.cpp release; fall back to the CPU
//!   build. Model: a BASE (non-instruct) Qwen2.5-1.5B GGUF, Q8_0.
//! - `Sidecar::start`: spawn `llama-server -m <model> --port <p> --host 127.0.0.1
//!   -ngl 99 -c 2048 --cache-prompt --slots 1 --parallel 1 --log-disable` (adjust flags to
//!   the release used); wait until GET /health is 200; kill on drop.
//! - `CompletionClient::complete`: POST /completion with `stream: true`, `n_predict`,
//!   `temperature: 0`, `stop` on sentence terminators + "\n", and `cache_prompt: true`.
//!   Streams tokens through `on_token`; honours `cancel` (drops the HTTP body). Returns
//!   the full text and mean token logprob when the server provides
//!   `completion_probabilities` (request `n_probs: 1`), else None.
//! - `prompt` is raw text (no chat template).

use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct Assets {
    pub server_exe: PathBuf,
    pub model: PathBuf,
}

/// Download/verify binaries and model under `root` (e.g. %LOCALAPPDATA%\CaseComplete).
/// `progress(label, done_bytes, total_bytes)`.
pub async fn ensure_assets(
    root: &Path,
    progress: impl FnMut(&str, u64, Option<u64>) + Send,
) -> anyhow::Result<Assets> {
    let _ = (root, progress);
    todo!("cc-engine: download llama-server release + model GGUF with sha256")
}

pub struct Sidecar {
    _private: (),
}

impl Sidecar {
    pub async fn start(assets: &Assets, port: u16) -> anyhow::Result<Sidecar> {
        let _ = (assets, port);
        todo!()
    }
    pub fn base_url(&self) -> String {
        todo!()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub text: String,
    pub mean_logprob: Option<f32>,
    pub stopped_at_sentence_end: bool,
    pub prompt_ms: f32,
    pub predicted_ms: f32,
}

#[derive(Clone)]
pub struct CompletionClient {
    _private: (),
}

impl CompletionClient {
    pub fn new(base_url: &str) -> CompletionClient {
        let _ = base_url;
        todo!()
    }

    pub async fn complete(
        &self,
        prompt: &str,
        n_predict: u32,
        cancel: CancellationToken,
        on_token: impl FnMut(&str) + Send,
    ) -> anyhow::Result<Completion> {
        let _ = (prompt, n_predict, cancel, on_token);
        todo!()
    }
}
