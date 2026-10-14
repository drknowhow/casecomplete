//! Provisioning of the `llama-server` binaries and the base GGUF model.
//!
//! Binaries come from the newest ggml-org/llama.cpp release that ships Windows x64
//! assets (CUDA preferred, CPU as fallback); the model is the Q8_0 GGUF of one of the BASE
//! (non-instruct) models listed in [`MODELS`] (Qwen2.5-1.5B by default) from a Hugging Face
//! mirror, verified against the LFS sha256.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::RANGE;
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Progress sink: `(label, done_bytes, total_bytes)`.
type Progress<'a> = &'a mut (dyn FnMut(&str, u64, Option<u64>) + Send);

const USER_AGENT: &str = concat!("cc-engine/", env!("CARGO_PKG_VERSION"));
const RELEASES_LATEST: &str = "https://api.github.com/repos/ggml-org/llama.cpp/releases/latest";
const RELEASES_LIST: &str = "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=30";

/// Id of the model [`ensure_assets`] provisions.
const DEFAULT_MODEL_ID: &str = "qwen2.5-1.5b";

/// A base (non-instruct) GGUF model the app can choose between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    /// Stable identifier (settings, CLI), e.g. `qwen2.5-1.5b`.
    pub id: &'static str,
    /// Human-readable name for the UI.
    pub label: &'static str,
    /// Publisher and country, for the UI.
    pub origin: &'static str,
    /// Hugging Face repo holding the GGUF quantisations.
    pub repo: &'static str,
    /// File name of the Q8_0 quant inside [`ModelSpec::repo`].
    pub file_remote: &'static str,
    /// File name used under `root/models` (lowercase, unique across [`MODELS`]).
    pub file_local: &'static str,
    /// Exact size in bytes; lets a warm start skip every network call.
    pub size: u64,
}

/// The selectable base models, in display order.
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "gemma-3-1b-pt",
        label: "Gemma 3 1B (Google)",
        origin: "Google, US",
        repo: "mradermacher/gemma-3-1b-pt-GGUF",
        file_remote: "gemma-3-1b-pt.Q8_0.gguf",
        file_local: "gemma-3-1b-pt-q8_0.gguf",
        size: 1_069_304_992,
    },
    ModelSpec {
        id: "smollm2-360m",
        label: "SmolLM2 360M (Hugging Face)",
        origin: "Hugging Face, FR",
        repo: "QuantFactory/SmolLM2-360M-GGUF",
        file_remote: "SmolLM2-360M.Q8_0.gguf",
        file_local: "smollm2-360m-q8_0.gguf",
        size: 386_404_352,
    },
    ModelSpec {
        id: "qwen2.5-1.5b",
        label: "Qwen2.5 1.5B (Alibaba)",
        origin: "Alibaba, CN",
        repo: "QuantFactory/Qwen2.5-1.5B-GGUF",
        file_remote: "Qwen2.5-1.5B.Q8_0.gguf",
        file_local: "qwen2.5-1.5b-q8_0.gguf",
        size: 1_646_572_608,
    },
];

/// Look a model up by [`ModelSpec::id`].
pub fn model_spec(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.id == id)
}

/// Cheap, network-free check that `model_id` is known and its file under `root/models`
/// exists with the expected size.
pub fn model_installed(root: &Path, model_id: &str) -> bool {
    model_spec(model_id).is_some_and(|spec| file_has_size(&root.join("models").join(spec.file_local), spec.size))
}

fn file_has_size(path: &Path, size: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == size)
}

/// Paths of the provisioned sidecar binary and model.
#[derive(Debug, Clone)]
pub struct Assets {
    pub server_exe: PathBuf,
    pub model: PathBuf,
}

/// Download/verify binaries and model under `root` (e.g. %LOCALAPPDATA%\CaseComplete).
/// `progress(label, done_bytes, total_bytes)`.
///
/// Layout: `root/bin/llama-server.exe` (+ DLLs, `VERSION` holding the release tag) and
/// `root/models/qwen2.5-1.5b-q8_0.gguf`. Existing complete files are reused without any
/// network access. Interrupted downloads resume from their `.part` file.
///
/// Equivalent to [`ensure_assets_for`] with the default Qwen2.5-1.5B model.
pub async fn ensure_assets(
    root: &Path,
    progress: impl FnMut(&str, u64, Option<u64>) + Send,
) -> anyhow::Result<Assets> {
    ensure_assets_for(root, DEFAULT_MODEL_ID, progress).await
}

/// Like [`ensure_assets`], but provisions the model `model_id` (see [`MODELS`]); the model
/// lands at `root/models/<file_local>`. An unknown id fails before any file or network
/// access.
pub async fn ensure_assets_for(
    root: &Path,
    model_id: &str,
    mut progress: impl FnMut(&str, u64, Option<u64>) + Send,
) -> anyhow::Result<Assets> {
    let spec = model_spec(model_id).ok_or_else(|| anyhow!("unknown model id {model_id:?}"))?;
    let progress: Progress<'_> = &mut progress;
    let bin_dir = root.join("bin");
    let models_dir = root.join("models");
    let server_exe = bin_dir.join("llama-server.exe");
    let model = models_dir.join(spec.file_local);

    tokio::fs::create_dir_all(&bin_dir).await.context("creating bin dir")?;
    tokio::fs::create_dir_all(&models_dir).await.context("creating models dir")?;

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    if !server_exe.is_file() {
        install_binaries(&client, root, &bin_dir, &mut *progress).await?;
    }
    if !model_present(&model, spec.size).await {
        install_model(&client, spec, &model, &mut *progress).await?;
    }
    Ok(Assets { server_exe, model })
}

// ---------------------------------------------------------------------------------------
// llama.cpp binaries
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    /// `sha256:<hex>`, present on current GitHub API responses.
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Selection {
    main: GhAsset,
    /// CUDA runtime DLLs matching `main` (CUDA variant only).
    cudart: Option<GhAsset>,
    /// Human-readable variant, e.g. `cuda-12.4` or `cpu`.
    variant: String,
}

/// `win-cuda-12.4-x64` -> `(12, 4)`.
fn parse_cuda_version(name: &str) -> Option<(u32, u32)> {
    let rest = &name[name.find("win-cuda-")? + "win-cuda-".len()..];
    let ver: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// Choose the Windows x64 CUDA zip (+ matching cudart zip), preferring CUDA 12.x, else the
/// CPU zip. `None` if the release has neither.
fn pick_assets(assets: &[GhAsset]) -> Option<Selection> {
    let is_zip = |a: &GhAsset| a.name.to_ascii_lowercase().ends_with(".zip");
    let mut best: Option<((bool, u32, u32), &GhAsset)> = None;
    for a in assets.iter().filter(|a| is_zip(a)) {
        let n = a.name.to_ascii_lowercase();
        if n.starts_with("cudart-") || !n.contains("bin-win-cuda") || !n.contains("x64") {
            continue;
        }
        let Some((maj, min)) = parse_cuda_version(&n) else { continue };
        let key = (maj == 12, maj, min);
        if best.as_ref().is_none_or(|(k, _)| key > *k) {
            best = Some((key, a));
        }
    }
    if let Some(((_, maj, min), main)) = best {
        let needle = format!("win-cuda-{maj}.{min}-x64");
        let cudart = assets
            .iter()
            .filter(|a| is_zip(a))
            .find(|a| {
                let n = a.name.to_ascii_lowercase();
                n.starts_with("cudart-") && n.contains(&needle)
            })
            .cloned();
        return Some(Selection { main: main.clone(), cudart, variant: format!("cuda-{maj}.{min}") });
    }
    assets
        .iter()
        .filter(|a| is_zip(a))
        .find(|a| {
            let n = a.name.to_ascii_lowercase();
            !n.starts_with("cudart-") && n.contains("bin-win-cpu-x64")
        })
        .map(|main| Selection { main: main.clone(), cudart: None, variant: "cpu".into() })
}

async fn get_json<T: serde::de::DeserializeOwned>(client: &reqwest::Client, url: &str) -> Result<T> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        bail!("GET {url}: HTTP {status}: {body}");
    }
    resp.json::<T>().await.with_context(|| format!("decoding JSON from {url}"))
}

/// The newest release that carries usable Windows x64 assets.
///
/// `/releases/latest` is tried first, as is conventional. ggml-org currently marks a
/// non-binary "v0.x" tag as latest (its only asset is `nightly-tag.txt`) while the real
/// binaries live in `bNNNN` releases, so when `latest` has nothing usable the most recent
/// releases are scanned instead.
async fn find_release(client: &reqwest::Client) -> Result<(String, Selection)> {
    match get_json::<GhRelease>(client, RELEASES_LATEST).await {
        Ok(rel) => {
            if let Some(sel) = pick_assets(&rel.assets) {
                return Ok((rel.tag_name, sel));
            }
            log::warn!(
                "llama.cpp release {} has no Windows x64 binaries; scanning recent releases",
                rel.tag_name
            );
        }
        Err(e) => log::warn!("could not read llama.cpp latest release ({e:#}); scanning recent releases"),
    }
    let list: Vec<GhRelease> = get_json(client, RELEASES_LIST).await?;
    for rel in list.into_iter().filter(|r| !r.draft) {
        if let Some(sel) = pick_assets(&rel.assets) {
            return Ok((rel.tag_name, sel));
        }
    }
    bail!("no llama.cpp release with Windows x64 CUDA or CPU binaries found")
}

async fn install_binaries(
    client: &reqwest::Client,
    root: &Path,
    bin_dir: &Path,
    progress: Progress<'_>,
) -> Result<()> {
    let (tag, sel) = find_release(client).await?;
    log::info!("installing llama.cpp {tag} ({}) into {}", sel.variant, bin_dir.display());
    if sel.variant != "cpu" && sel.cudart.is_none() {
        log::warn!("no cudart zip found for {}; relying on a system CUDA runtime", sel.variant);
    }

    let dl_dir = root.join("downloads");
    tokio::fs::create_dir_all(&dl_dir).await.context("creating downloads dir")?;
    let staging = root.join("bin.staging");
    if staging.exists() {
        tokio::fs::remove_dir_all(&staging).await.context("clearing staging dir")?;
    }
    tokio::fs::create_dir_all(&staging).await.context("creating staging dir")?;

    for asset in std::iter::once(&sel.main).chain(sel.cudart.as_ref()) {
        let part = dl_dir.join(format!("{}.part", asset.name));
        let zip_path = dl_dir.join(&asset.name);
        let label = format!("download {}", asset.name);
        download(client, &asset.browser_download_url, &part, &label, Some(asset.size), &mut *progress)
            .await?;
        if let Some(want) = asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")) {
            let got = sha256_file(&part, &format!("verify {}", asset.name), &mut *progress).await?;
            if !got.eq_ignore_ascii_case(want) {
                let _ = tokio::fs::remove_file(&part).await;
                bail!("sha256 mismatch for {}: expected {want}, got {got}", asset.name);
            }
        }
        tokio::fs::rename(&part, &zip_path).await.context("finalising download")?;

        let label = format!("extract {}", asset.name);
        progress(&label, 0, None);
        let (zp, dest) = (zip_path.clone(), staging.clone());
        let n = tokio::task::spawn_blocking(move || extract_zip(&zp, &dest))
            .await
            .context("extract task panicked")??;
        progress(&label, n as u64, Some(n as u64));
        let _ = tokio::fs::remove_file(&zip_path).await;
    }

    if !staging.join("llama-server.exe").is_file() {
        bail!("llama.cpp {tag} archive did not contain llama-server.exe");
    }
    tokio::fs::write(staging.join("VERSION"), format!("{tag}\n")).await.context("writing VERSION")?;

    // Replace the (incomplete, since llama-server.exe is missing) bin dir atomically.
    if bin_dir.exists() {
        tokio::fs::remove_dir_all(bin_dir).await.context("removing incomplete bin dir")?;
    }
    tokio::fs::rename(&staging, bin_dir).await.context("moving staged binaries into place")?;
    Ok(())
}

/// If every entry sits under one common top-level directory, return its name.
fn common_top_dir(entries: &[(PathBuf, bool)]) -> Option<OsString> {
    let mut top: Option<OsString> = None;
    for (path, is_dir) in entries {
        let mut comps = path.components().filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        });
        let first = comps.next()?;
        if comps.next().is_none() && !*is_dir {
            return None; // a file directly at the archive root: nothing to strip
        }
        match &top {
            None => top = Some(first.to_os_string()),
            Some(t) if t.as_os_str() == first => {}
            Some(_) => return None,
        }
    }
    top
}

/// Extract `zip_path` into `dest`, dropping a single wrapping directory if present.
/// Entries with unsafe paths (absolute, `..`) are skipped. Returns the file count.
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<usize> {
    let file = std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("reading {}", zip_path.display()))?;

    let mut entries: Vec<(usize, PathBuf, bool)> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        match entry.enclosed_name() {
            Some(p) => entries.push((i, p, entry.is_dir())),
            None => log::warn!("skipping unsafe zip entry {:?}", entry.name()),
        }
    }
    let flags: Vec<(PathBuf, bool)> = entries.iter().map(|(_, p, d)| (p.clone(), *d)).collect();
    let strip = common_top_dir(&flags);

    let mut files = 0;
    for (i, path, is_dir) in entries {
        let rel = match &strip {
            Some(top) => path.strip_prefix(top).map(Path::to_path_buf).unwrap_or(path),
            None => path,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if is_dir {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut entry = archive.by_index(i)?;
        let mut w = std::fs::File::create(&out).with_context(|| format!("creating {}", out.display()))?;
        std::io::copy(&mut entry, &mut w).with_context(|| format!("extracting {}", out.display()))?;
        files += 1;
    }
    Ok(files)
}

// ---------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct HfEntry {
    path: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    lfs: Option<HfLfs>,
}

#[derive(Deserialize)]
struct HfLfs {
    oid: String,
    #[serde(default)]
    size: Option<u64>,
}

async fn model_present(model: &Path, size: u64) -> bool {
    tokio::fs::metadata(model).await.is_ok_and(|m| m.is_file() && m.len() == size)
}

/// `(sha256, size)` of the model file according to the Hugging Face LFS metadata.
async fn fetch_model_meta(client: &reqwest::Client, spec: &ModelSpec) -> Result<(String, u64)> {
    let (repo, file) = (spec.repo, spec.file_remote);
    let url = format!("https://huggingface.co/api/models/{repo}/tree/main");
    let entries: Vec<HfEntry> = get_json(client, &url).await?;
    let entry = entries
        .into_iter()
        .find(|e| e.path == file)
        .ok_or_else(|| anyhow!("{file} not found in Hugging Face repo {repo}"))?;
    let lfs = entry.lfs.ok_or_else(|| anyhow!("{file} has no LFS metadata"))?;
    Ok((lfs.oid.to_ascii_lowercase(), lfs.size.or(entry.size).unwrap_or(spec.size)))
}

async fn install_model(
    client: &reqwest::Client,
    spec: &ModelSpec,
    model: &Path,
    progress: Progress<'_>,
) -> Result<()> {
    let (repo, file) = (spec.repo, spec.file_remote);
    let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");

    // The repo and file must really exist before we start a multi-hundred-MB transfer.
    let head = client
        .head(&url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("HEAD {url}"))?;
    if !head.status().is_success() {
        bail!("HEAD {url}: HTTP {}", head.status());
    }
    let (want_sha, want_size) = fetch_model_meta(client, spec).await?;
    if want_size != spec.size {
        log::warn!("{file}: Hugging Face reports {want_size} bytes, expected {}", spec.size);
    }

    let part = model.with_extension("gguf.part");
    let name = model.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    download(client, &url, &part, &format!("download {name}"), Some(want_size), &mut *progress).await?;

    let got = sha256_file(&part, &format!("verify {name}"), &mut *progress).await?;
    if got != want_sha {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("sha256 mismatch for {file}: expected {want_sha}, got {got}");
    }
    tokio::fs::rename(&part, model).await.context("moving model into place")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Transfer helpers
// ---------------------------------------------------------------------------------------

/// Rate limiter for progress callbacks (~10 Hz).
struct Throttle(Option<Instant>);

impl Throttle {
    fn ready(&mut self) -> bool {
        let now = Instant::now();
        match self.0 {
            Some(t) if now.duration_since(t) < Duration::from_millis(100) => false,
            _ => {
                self.0 = Some(now);
                true
            }
        }
    }
}

/// Stream `url` into `part`, resuming from an existing partial file when the server
/// supports ranges. Fails (leaving `part` for a later resume) on a short transfer.
async fn download(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    label: &str,
    expected: Option<u64>,
    progress: Progress<'_>,
) -> Result<()> {
    let mut have = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    if let Some(total) = expected {
        if have > total {
            have = 0;
        }
        if have == total && have > 0 {
            progress(label, have, expected);
            return Ok(());
        }
    }

    let mut req = client.get(url);
    if have > 0 {
        req = req.header(RANGE, format!("bytes={have}-"));
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();

    let (mut done, mut file) = if status == StatusCode::PARTIAL_CONTENT && have > 0 {
        let f = tokio::fs::OpenOptions::new().append(true).open(part).await?;
        (have, f)
    } else if status.is_success() {
        (0, tokio::fs::File::create(part).await?)
    } else {
        bail!("GET {url}: HTTP {status}");
    };
    let total = expected.or_else(|| resp.content_length().map(|l| l + done));

    let mut throttle = Throttle(None);
    progress(label, done, total);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("downloading {url}"))?;
        file.write_all(&chunk).await.with_context(|| format!("writing {}", part.display()))?;
        done += chunk.len() as u64;
        if throttle.ready() {
            progress(label, done, total);
        }
    }
    file.flush().await?;
    drop(file);
    progress(label, done, total);
    if let Some(t) = total {
        if done != t {
            bail!("incomplete download of {url}: {done} of {t} bytes");
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

async fn sha256_file(path: &Path, label: &str, progress: Progress<'_>) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
    let total = file.metadata().await?.len();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    let mut throttle = Throttle(None);
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        if throttle.ready() {
            progress(label, done, Some(total));
        }
    }
    progress(label, done, Some(total));
    Ok(hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn asset(name: &str) -> GhAsset {
        GhAsset {
            name: name.into(),
            browser_download_url: format!("https://example.invalid/{name}"),
            size: 1,
            digest: None,
        }
    }

    fn b11620() -> Vec<GhAsset> {
        [
            "cudart-llama-b11620-bin-ubuntu-cuda-12.8-x64.tar.gz",
            "cudart-llama-bin-win-cuda-12.4-x64.zip",
            "cudart-llama-bin-win-cuda-13.4-arm64.zip",
            "cudart-llama-bin-win-cuda-13.4-x64.zip",
            "llama-b11620-bin-ubuntu-cuda-12.8-x64.tar.gz",
            "llama-b11620-bin-win-cpu-arm64.zip",
            "llama-b11620-bin-win-cpu-x64.zip",
            "llama-b11620-bin-win-cuda-12.4-x64.zip",
            "llama-b11620-bin-win-cuda-13.4-arm64.zip",
            "llama-b11620-bin-win-cuda-13.4-x64.zip",
            "llama-b11620-bin-win-vulkan-x64.zip",
        ]
        .iter()
        .map(|n| asset(n))
        .collect()
    }

    #[test]
    fn cuda_version_parsing() {
        assert_eq!(parse_cuda_version("llama-b1-bin-win-cuda-12.4-x64.zip"), Some((12, 4)));
        assert_eq!(parse_cuda_version("cudart-llama-bin-win-cuda-13.4-x64.zip"), Some((13, 4)));
        assert_eq!(parse_cuda_version("llama-b1-bin-win-cpu-x64.zip"), None);
    }

    #[test]
    fn prefers_cuda_12_with_matching_cudart() {
        let sel = pick_assets(&b11620()).unwrap();
        assert_eq!(sel.main.name, "llama-b11620-bin-win-cuda-12.4-x64.zip");
        assert_eq!(sel.cudart.unwrap().name, "cudart-llama-bin-win-cuda-12.4-x64.zip");
        assert_eq!(sel.variant, "cuda-12.4");
    }

    #[test]
    fn falls_back_to_newest_cuda_when_no_12() {
        let assets: Vec<_> = b11620().into_iter().filter(|a| !a.name.contains("cuda-12.4")).collect();
        let sel = pick_assets(&assets).unwrap();
        assert_eq!(sel.main.name, "llama-b11620-bin-win-cuda-13.4-x64.zip");
        assert_eq!(sel.cudart.unwrap().name, "cudart-llama-bin-win-cuda-13.4-x64.zip");
    }

    #[test]
    fn falls_back_to_cpu_without_cuda() {
        let assets: Vec<_> = b11620().into_iter().filter(|a| !a.name.contains("cuda")).collect();
        let sel = pick_assets(&assets).unwrap();
        assert_eq!(sel.main.name, "llama-b11620-bin-win-cpu-x64.zip");
        assert!(sel.cudart.is_none());
        assert_eq!(sel.variant, "cpu");
    }

    #[test]
    fn nothing_usable_in_non_binary_release() {
        assert!(pick_assets(&[asset("nightly-tag.txt")]).is_none());
    }

    #[test]
    fn hex_encoding() {
        assert_eq!(hex(&[0x00, 0xab, 0x0f, 0xff]), "00ab0fff");
    }

    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-engine-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extracts_flat_zip() {
        let dir = scratch("flat");
        let zip_path = dir.join("a.zip");
        make_zip(&zip_path, &[("llama-server.exe", b"exe"), ("ggml.dll", b"dll")]);
        let out = dir.join("out");
        assert_eq!(extract_zip(&zip_path, &out).unwrap(), 2);
        assert_eq!(std::fs::read(out.join("llama-server.exe")).unwrap(), b"exe");
        assert!(out.join("ggml.dll").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strips_single_wrapping_directory() {
        let dir = scratch("wrapped");
        let zip_path = dir.join("a.zip");
        make_zip(&zip_path, &[("build/llama-server.exe", b"exe"), ("build/sub/x.dll", b"dll")]);
        let out = dir.join("out");
        extract_zip(&zip_path, &out).unwrap();
        assert!(out.join("llama-server.exe").is_file());
        assert!(out.join("sub").join("x.dll").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_archives_merge_into_one_dir() {
        let dir = scratch("merge");
        let (a, b) = (dir.join("a.zip"), dir.join("b.zip"));
        make_zip(&a, &[("llama-server.exe", b"exe")]);
        make_zip(&b, &[("cudart64_12.dll", b"dll")]);
        let out = dir.join("out");
        extract_zip(&a, &out).unwrap();
        extract_zip(&b, &out).unwrap();
        assert!(out.join("llama-server.exe").is_file());
        assert!(out.join("cudart64_12.dll").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_spec_lookup() {
        let q = model_spec("qwen2.5-1.5b").unwrap();
        assert_eq!(q.file_local, "qwen2.5-1.5b-q8_0.gguf");
        assert_eq!(q.file_remote, "Qwen2.5-1.5B.Q8_0.gguf");
        assert_eq!(q.repo, "QuantFactory/Qwen2.5-1.5B-GGUF");
        assert_eq!(q.size, 1_646_572_608);
        assert_eq!(model_spec("gemma-3-1b-pt").unwrap().label, "Gemma 3 1B (Google)");
        assert_eq!(model_spec("smollm2-360m").unwrap().origin, "Hugging Face, FR");
        assert!(model_spec("nope").is_none());
        assert!(model_spec("").is_none());
        assert!(model_spec("Qwen2.5-1.5B").is_none(), "ids are case-sensitive");
        assert!(model_spec(DEFAULT_MODEL_ID).is_some());
        for m in MODELS {
            assert_eq!(model_spec(m.id), Some(m));
        }
    }

    #[test]
    fn models_are_in_the_documented_order() {
        let ids: Vec<_> = MODELS.iter().map(|m| m.id).collect();
        assert_eq!(ids, ["gemma-3-1b-pt", "smollm2-360m", "qwen2.5-1.5b"]);
    }

    #[test]
    fn model_specs_are_well_formed() {
        let mut ids = std::collections::HashSet::new();
        let mut locals = std::collections::HashSet::new();
        for m in MODELS {
            assert!(ids.insert(m.id), "duplicate id {}", m.id);
            assert!(locals.insert(m.file_local), "duplicate file_local {}", m.file_local);
            assert_eq!(m.file_local, m.file_local.to_ascii_lowercase(), "{} not lowercase", m.file_local);
            assert!(m.file_local.ends_with(".gguf") && m.file_remote.ends_with(".gguf"));
            assert!(!m.file_local.contains(['/', '\\']), "{} must be a bare file name", m.file_local);
            assert!(m.repo.contains('/'));
            assert!(m.size > 0);
        }
    }

    #[test]
    fn model_installed_checks_presence_and_size() {
        let dir = scratch("installed");
        assert!(!model_installed(&dir, "qwen2.5-1.5b"), "no models dir yet");
        assert!(!model_installed(&dir, "nope"));
        let models = dir.join("models");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("qwen2.5-1.5b-q8_0.gguf"), b"truncated").unwrap();
        assert!(!model_installed(&dir, "qwen2.5-1.5b"), "wrong size must not count as installed");
        assert!(!model_installed(&dir, "smollm2-360m"), "other models are independent");

        let small = dir.join("small.bin");
        std::fs::write(&small, b"12345").unwrap();
        assert!(file_has_size(&small, 5));
        assert!(!file_has_size(&small, 4));
        assert!(!file_has_size(&dir, 5), "a directory is not a model file");
        assert!(!file_has_size(&dir.join("missing"), 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_model_id_fails_before_any_io() {
        use futures_util::FutureExt;
        let dir = scratch("unknown-id");
        let res = ensure_assets_for(&dir, "nope", |_, _, _| {}).now_or_never().expect("must not await");
        let err = res.unwrap_err().to_string();
        assert!(err.contains("unknown model id"), "{err}");
        assert!(!dir.join("bin").exists() && !dir.join("models").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_path_traversal_entries() {
        let dir = scratch("slip");
        let zip_path = dir.join("a.zip");
        make_zip(&zip_path, &[("../evil.txt", b"x"), ("ok.txt", b"y")]);
        let out = dir.join("out");
        assert_eq!(extract_zip(&zip_path, &out).unwrap(), 1);
        assert!(!dir.join("evil.txt").exists());
        assert!(out.join("ok.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
