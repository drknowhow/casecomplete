//! `--download`: fetch extra BASE-model GGUFs into the models directory.
//!
//! cc-engine's downloader is private, so this is a small copy of its approach: probe the
//! Hugging Face `resolve` URL with a HEAD request, stream the body to `<name>.part` while
//! hashing, verify size and the LFS sha256 (`x-linked-etag`), then rename.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, LOCATION};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const USER_AGENT: &str = concat!("cc-bench/", env!("CARGO_PKG_VERSION"));

/// A model to provision: the local file name plus `(repo, file)` candidates tried in order.
pub struct Spec {
    pub label: &'static str,
    pub local: &'static str,
    pub candidates: &'static [(&'static str, &'static str)],
}

/// Q8_0 GGUFs of BASE (pretrained, not instruct) models. The first candidate of each is
/// the conventional unsloth/bartowski repo; later ones are fallbacks. A candidate that does
/// not exist answers HEAD with 401/404 and is skipped.
pub const SPECS: &[Spec] = &[
    Spec {
        label: "Qwen3-1.7B-Base Q8_0",
        local: "qwen3-1.7b-base-q8_0.gguf",
        candidates: &[
            ("unsloth/Qwen3-1.7B-Base-GGUF", "Qwen3-1.7B-Base-Q8_0.gguf"),
            ("mradermacher/Qwen3-1.7B-Base-GGUF", "Qwen3-1.7B-Base.Q8_0.gguf"),
        ],
    },
    Spec {
        label: "gemma-3-1b-pt Q8_0",
        local: "gemma-3-1b-pt-q8_0.gguf",
        candidates: &[
            ("unsloth/gemma-3-1b-pt-GGUF", "gemma-3-1b-pt-Q8_0.gguf"),
            ("bartowski/google_gemma-3-1b-pt-GGUF", "google_gemma-3-1b-pt-Q8_0.gguf"),
            ("mradermacher/gemma-3-1b-pt-GGUF", "gemma-3-1b-pt.Q8_0.gguf"),
            ("gguf-org/gemma-3-1b-pt-gguf", "gemma-3-1b-pt-q8_0.gguf"),
        ],
    },
    Spec {
        label: "SmolLM2-360M Q8_0",
        local: "smollm2-360m-q8_0.gguf",
        candidates: &[
            ("bartowski/SmolLM2-360M-GGUF", "SmolLM2-360M-Q8_0.gguf"),
            ("QuantFactory/SmolLM2-360M-GGUF", "SmolLM2-360M.Q8_0.gguf"),
        ],
    },
];

#[derive(Debug)]
struct Remote {
    url: String,
    repo: &'static str,
    file: &'static str,
    size: Option<u64>,
    sha256: Option<String>,
}

pub fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Make sure every spec's file exists in `models_dir`; print what happened for each.
pub async fn run(models_dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(models_dir)
        .await
        .with_context(|| format!("creating {}", models_dir.display()))?;
    let head = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let get = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .build()?;

    println!("== download ==");
    for spec in SPECS {
        let dest = models_dir.join(spec.local);
        if let Ok(meta) = std::fs::metadata(&dest) {
            if meta.len() > 0 {
                println!("{}: already present ({:.2} GiB) {}", spec.label, gib(meta.len()), dest.display());
                continue;
            }
        }
        let mut found = None;
        for (repo, file) in spec.candidates {
            match probe(&head, repo, file).await {
                Ok(r) => {
                    found = Some(r);
                    break;
                }
                Err(why) => println!("  {repo}/{file}: {why}"),
            }
        }
        let Some(remote) = found else {
            println!("{}: NOT FOUND in any candidate repo, skipped", spec.label);
            continue;
        };
        println!(
            "{}: found {}/{} ({}), downloading",
            spec.label,
            remote.repo,
            remote.file,
            remote.size.map_or("size unknown".to_string(), |s| format!("{:.2} GiB", gib(s)))
        );
        match fetch(&get, &remote, &dest).await {
            Ok(size) => println!("{}: downloaded {:.2} GiB -> {}", spec.label, gib(size), dest.display()),
            Err(e) => {
                let _ = std::fs::remove_file(part_path(&dest));
                println!("{}: FAILED: {e:#}", spec.label);
            }
        }
    }
    Ok(())
}

fn part_path(dest: &Path) -> std::path::PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// HEAD the `resolve` URL (without following redirects): LFS files answer 302 with the real
/// size and sha256 in `x-linked-size` / `x-linked-etag`.
async fn probe(head: &reqwest::Client, repo: &'static str, file: &'static str) -> Result<Remote, String> {
    let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");
    let resp = head.head(&url).send().await.map_err(|e| format!("HEAD failed: {e}"))?;
    let status = resp.status();
    let exists = status.is_success() || (status.is_redirection() && resp.headers().contains_key(LOCATION));
    if !exists {
        return Err(format!("HEAD -> HTTP {status}"));
    }
    let h = resp.headers();
    let size = header_u64(h, "x-linked-size").or_else(|| status.is_success().then(|| header_u64(h, "content-length")).flatten());
    let sha256 = h
        .get("x-linked-etag")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start_matches("W/").trim_matches('"').to_ascii_lowercase())
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()));
    Ok(Remote { url, repo, file, size, sha256 })
}

fn header_u64(h: &HeaderMap, name: &str) -> Option<u64> {
    h.get(name)?.to_str().ok()?.parse().ok()
}

/// Stream to `<dest>.part`, check size and sha256, rename. Returns the byte count.
async fn fetch(get: &reqwest::Client, remote: &Remote, dest: &Path) -> Result<u64> {
    let part = part_path(dest);
    let resp = get.get(&remote.url).send().await.context("GET")?.error_for_status()?;
    let total = resp.content_length().or(remote.size);
    let mut file = tokio::fs::File::create(&part).await.with_context(|| format!("creating {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut last_print = Instant::now();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download stream")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await.context("writing .part file")?;
        done += chunk.len() as u64;
        if last_print.elapsed() >= Duration::from_secs(5) {
            last_print = Instant::now();
            match total {
                Some(t) => eprintln!("  {}: {:.0}% ({:.2}/{:.2} GiB)", remote.file, 100.0 * done as f64 / t as f64, gib(done), gib(t)),
                None => eprintln!("  {}: {:.2} GiB", remote.file, gib(done)),
            }
        }
    }
    file.flush().await?;
    drop(file);
    if let Some(t) = total {
        if done != t {
            bail!("size mismatch: got {done} bytes, expected {t}");
        }
    }
    if let Some(expected) = &remote.sha256 {
        let got = format!("{:x}", hasher.finalize());
        if &got != expected {
            bail!("sha256 mismatch: got {got}, expected {expected}");
        }
    } else {
        println!("  (no sha256 published for {}, size check only)", remote.file);
    }
    tokio::fs::rename(&part, dest).await.with_context(|| format!("renaming {} -> {}", part.display(), dest.display()))?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_file_sits_next_to_the_target() {
        assert_eq!(part_path(Path::new("m/a.gguf")), Path::new("m/a.gguf.part"));
    }

    #[test]
    fn specs_are_q8_gguf_with_candidates() {
        for s in SPECS {
            assert!(s.local.ends_with("q8_0.gguf"), "{}", s.local);
            assert!(!s.candidates.is_empty());
            for (_, f) in s.candidates {
                assert!(f.to_ascii_lowercase().contains("q8_0") && f.ends_with(".gguf"), "{f}");
            }
        }
    }
}
