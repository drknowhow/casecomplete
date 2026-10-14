//! Completion engine: manages the bundled `llama-server` sidecar and streams completions.
//!
//! Contract:
//! - `ensure_assets(root)`: make sure `root/bin/llama-server.exe` (+ DLLs) and
//!   `root/models/<default>.gguf` exist; download with progress + sha256 check if not.
//!   Prefer the CUDA build (12.x first) of a recent ggml-org/llama.cpp release; fall back to
//!   the CPU build. Model: a BASE (non-instruct) Qwen2.5-1.5B GGUF, Q8_0.
//! - `ensure_assets_for(root, model_id)`: same, for any entry of `MODELS` (looked up with
//!   `model_spec`); `model_installed` is the cheap offline check for one of them.
//! - `Sidecar::start`: spawn `llama-server -m <model> --host 127.0.0.1 --port <p> -ngl 99
//!   -c 2048 -np 1 --cache-prompt`; wait until GET /health is 200; kill on drop.
//! - `CompletionClient::complete`: POST /completion with `stream: true`, `n_predict`,
//!   `temperature: 0`, `stop: ["\n"]` and `cache_prompt: true`. `. ! ?` are not server-side
//!   stops (they would be stripped from the output); the client ends the request itself once
//!   the text finishes a sentence. Streams tokens through `on_token`; honours `cancel`
//!   (drops the HTTP body). Returns the full text and mean token logprob when the server
//!   provides `completion_probabilities` (request `n_probs: 1`), else None.
//! - `prompt` is raw text (no chat template).

mod assets;
mod completion;
mod sidecar;

pub use assets::{ensure_assets, ensure_assets_for, model_installed, model_spec, Assets, ModelSpec, MODELS};
pub use completion::{Completion, CompletionClient};
pub use sidecar::Sidecar;
