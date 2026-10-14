//! cc-bench: compares small base LLMs as typing predictors.
//!
//! For every sentence of an evenly sampled eval set and every cut point (3, 6 and 9 words
//! typed), the prompt is the previous two sentences of the same file, a newline and the
//! typed prefix. The model completes it with the app's stop behaviour (newline stop,
//! client-side sentence-end detection) and the result is scored against the real text.
//!
//! ```text
//! cargo run --release -p cc-bench -- [corpus-dir] [--model a.gguf]... [--download]
//! ```
//! Run with `--help` for all options.

mod corpus;
mod download;
mod metrics;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use cc_engine::{Assets, CompletionClient, Sidecar};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use corpus::{CorpusStats, Example};
use metrics::{score, summarize, Row, Summary};

/// Words typed before the cut.
const CUTS: [usize; 3] = [3, 6, 9];
/// The cut must leave at least this many words, so every metric is defined.
const WORDS_AFTER_CUT: usize = 3;
const DEFAULT_PORT: u16 = 8097;
const DEFAULT_N_PREDICT: u32 = 8;
const DEFAULT_SENTENCES: usize = 150;
const WARMUP_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_ERRORS: usize = 5;

const USAGE: &str = "\
cc-bench: compare small base LLMs as typing predictors

USAGE:
    cc-bench [CORPUS_DIR] [OPTIONS]

ARGS:
    CORPUS_DIR            directory (or file) with *.md / *.txt  [default: <repo>/docs]

OPTIONS:
    --model <file.gguf>   model to benchmark; repeatable  [default: every *.gguf in <root>/models]
    -n, --sentences <N>   eval sentences, evenly spaced  [default: 150]
    --download            first fetch missing Qwen3-1.7B-Base, gemma-3-1b-pt and SmolLM2-360M (Q8_0)
    --download-only       with --download: stop after downloading
    --root <dir>          holds bin/llama-server.exe and models/  [default: %LOCALAPPDATA%\\CaseComplete]
    --port <p>            sidecar port  [default: 8097]
    --n-predict <n>       tokens per completion  [default: 8]
    --out-dir <dir>       where the result JSON goes  [default: tools/bench/results]
    --trailing-space      end the prompt with a space after the last typed word (BPE models
                          handle this poorly: top-1 collapses to ~1%)
    --dry-run             load the corpus, print the plan, run nothing
    -h, --help            this text
";

struct Args {
    corpus: PathBuf,
    models: Vec<PathBuf>,
    sentences: usize,
    root: PathBuf,
    port: u16,
    n_predict: u32,
    out_dir: PathBuf,
    download: bool,
    download_only: bool,
    dry_run: bool,
    trailing_space: bool,
}

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn default_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("CaseComplete")
}

fn parse_args() -> Result<Args> {
    let repo_root = manifest_dir().ancestors().nth(2).unwrap_or(manifest_dir());
    let mut args = Args {
        corpus: repo_root.join("docs"),
        models: Vec::new(),
        sentences: DEFAULT_SENTENCES,
        root: default_root(),
        port: DEFAULT_PORT,
        n_predict: DEFAULT_N_PREDICT,
        out_dir: manifest_dir().join("results"),
        download: false,
        download_only: false,
        dry_run: false,
        trailing_space: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().with_context(|| format!("{flag} needs a value"));
        match a.as_str() {
            "--model" | "-m" => args.models.push(PathBuf::from(value("--model")?)),
            "--sentences" | "-n" => args.sentences = value("--sentences")?.parse().context("--sentences")?,
            "--root" => args.root = PathBuf::from(value("--root")?),
            "--port" => args.port = value("--port")?.parse().context("--port")?,
            "--n-predict" => args.n_predict = value("--n-predict")?.parse().context("--n-predict")?,
            "--out-dir" => args.out_dir = PathBuf::from(value("--out-dir")?),
            "--download" => args.download = true,
            "--download-only" => {
                args.download = true;
                args.download_only = true;
            }
            "--dry-run" => args.dry_run = true,
            "--trailing-space" => args.trailing_space = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            s if s.starts_with('-') => bail!("unknown option {s}\n\n{USAGE}"),
            _ => args.corpus = PathBuf::from(&a),
        }
    }
    if args.port == 8089 {
        bail!("port 8089 belongs to the running CaseComplete app; pick another");
    }
    Ok(args)
}

// ---------------------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------------------

/// One prompt to send: a sentence cut after `cut` words.
struct Case {
    file: String,
    sentence_index: usize,
    cut: usize,
    sentence: String,
    prompt: String,
    /// The real continuation (with its leading space when the prompt has no trailing space).
    truth: String,
}

/// Prompt = previous two sentences + "\n" + prefix (just the prefix when the file offers no
/// earlier sentence). The prefix is cut right after the last typed word, so the model's
/// completion starts with the space (` fox jumps ...`); that space is part of the true
/// continuation and counts as a saved character. With `trailing_space` the prompt instead
/// ends in a space and the continuation starts at the next word.
fn build_cases(examples: &[Example], trailing_space: bool) -> Vec<Case> {
    let mut cases = Vec::new();
    for ex in examples {
        let words: Vec<&str> = ex.sentence.split_whitespace().collect();
        for cut in CUTS {
            if words.len() < cut + WORDS_AFTER_CUT {
                continue;
            }
            let prefix = words[..cut].join(" ");
            let rest = words[cut..].join(" ");
            let mut prompt = if ex.context.is_empty() {
                prefix
            } else {
                format!("{}\n{prefix}", ex.context.join(" "))
            };
            let truth = if trailing_space {
                prompt.push(' ');
                rest
            } else {
                format!(" {rest}")
            };
            cases.push(Case {
                file: ex.file.clone(),
                sentence_index: ex.index,
                cut,
                sentence: ex.sentence.clone(),
                prompt,
                truth,
            });
        }
    }
    cases
}

// ---------------------------------------------------------------------------------------
// Running a model
// ---------------------------------------------------------------------------------------

#[derive(Serialize)]
struct ModelRun {
    name: String,
    path: String,
    size_bytes: u64,
    startup_ms: Option<f64>,
    warmup_ms: Option<f64>,
    /// Set when the model could not be benchmarked (or the run was aborted part-way).
    error: Option<String>,
    summary: Summary,
    rows: Vec<Row>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

async fn run_model(args: &Args, server_exe: &Path, model: &Path, cases: &[Case]) -> ModelRun {
    let mut run = ModelRun {
        name: model.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        path: model.display().to_string(),
        size_bytes: std::fs::metadata(model).map(|m| m.len()).unwrap_or(0),
        startup_ms: None,
        warmup_ms: None,
        error: None,
        summary: Summary::default(),
        rows: Vec::with_capacity(cases.len()),
    };
    if let Err(e) = measure(args, server_exe, model, cases, &mut run).await {
        run.error = Some(format!("{e:#}"));
    }
    run.summary = summarize(&run.rows);
    run
}

async fn measure(args: &Args, server_exe: &Path, model: &Path, cases: &[Case], run: &mut ModelRun) -> Result<()> {
    let assets = Assets { server_exe: server_exe.to_path_buf(), model: model.to_path_buf() };
    let t = Instant::now();
    let sidecar = start_sidecar(&assets, args.port).await?;
    run.startup_ms = Some(ms(t.elapsed()));
    let client = CompletionClient::new(&sidecar.base_url());

    let t = Instant::now();
    tokio::time::timeout(
        WARMUP_TIMEOUT,
        client.complete("The meeting is scheduled for tomorrow morning, and we", args.n_predict, CancellationToken::new(), |_| {}),
    )
    .await
    .context("warmup request did not finish within 30 s")?
    .context("warmup request failed")?;
    run.warmup_ms = Some(ms(t.elapsed()));

    let t_all = Instant::now();
    let mut consecutive_errors = 0;
    for (i, case) in cases.iter().enumerate() {
        let row = one_call(&client, case, args.n_predict).await;
        if row.error.is_some() {
            consecutive_errors += 1;
        } else {
            consecutive_errors = 0;
        }
        run.rows.push(row);
        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            let last = run.rows.last().and_then(|r| r.error.clone()).unwrap_or_default();
            bail!("aborted after {MAX_CONSECUTIVE_ERRORS} consecutive request errors (last: {last})");
        }
        if (i + 1) % 50 == 0 || i + 1 == cases.len() {
            let s = summarize(&run.rows);
            eprintln!(
                "  [{}/{}] top-1 {:.1}%  next-3 {:.1}%  {:.0}s elapsed",
                i + 1,
                cases.len(),
                s.top1_pct,
                s.next3_pct,
                t_all.elapsed().as_secs_f64()
            );
        }
    }
    Ok(())
}

/// A stopped sidecar can hold the port for a moment; retry while it is merely "busy".
async fn start_sidecar(assets: &Assets, port: u16) -> Result<Sidecar> {
    let mut attempt = 0;
    loop {
        match Sidecar::start(assets, port).await {
            Err(e) if attempt < 20 && format!("{e:#}").contains("is not available") => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            other => return other,
        }
    }
}

async fn one_call(client: &CompletionClient, case: &Case, n_predict: u32) -> Row {
    let t0 = Instant::now();
    let mut n_tokens = 0u32;
    let mut first_token: Option<Instant> = None;
    let res = tokio::time::timeout(
        CALL_TIMEOUT,
        client.complete(&case.prompt, n_predict, CancellationToken::new(), |_| {
            n_tokens += 1;
            if first_token.is_none() {
                first_token = Some(Instant::now());
            }
        }),
    )
    .await;
    let wall_ms = ms(t0.elapsed());
    let ttft_ms = first_token.map(|t| ms(t.duration_since(t0)));
    let (completion, error) = match res {
        Ok(Ok(c)) => (Some(c), None),
        Ok(Err(e)) => (None, Some(format!("{e:#}"))),
        Err(_) => (None, Some(format!("no result within {} s", CALL_TIMEOUT.as_secs()))),
    };
    let c = completion.unwrap_or_default();
    let sc = score(&c.text, &case.truth);
    Row {
        file: case.file.clone(),
        sentence_index: case.sentence_index,
        cut: case.cut,
        sentence: case.sentence.clone(),
        prompt: case.prompt.clone(),
        truth: case.truth.clone(),
        completion: c.text,
        top1: sc.top1,
        next3: sc.next3,
        chars_saved: sc.chars_saved,
        n_tokens,
        prompt_ms: c.prompt_ms,
        predicted_ms: c.predicted_ms,
        ttft_ms,
        wall_ms,
        mean_logprob: c.mean_logprob,
        stopped_at_sentence_end: c.stopped_at_sentence_end,
        error,
    }
}

// ---------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------

fn print_table(runs: &[ModelRun]) {
    let heads = ["model", "next-word top-1 %", "next-3 exact %", "mean chars saved", "ms/token", "p50 TTFT ms", "n"];
    let mut rows: Vec<[String; 7]> = Vec::new();
    for r in runs {
        let s = &r.summary;
        if s.calls == s.errors {
            let why = r.error.as_deref().unwrap_or("no results").lines().next().unwrap_or("").to_string();
            rows.push([r.name.clone(), format!("FAILED: {why}"), "-".into(), "-".into(), "-".into(), "-".into(), "0".into()]);
            continue;
        }
        let mark = if r.error.is_some() { "*" } else { "" };
        rows.push([
            format!("{}{mark}", r.name),
            format!("{:.1}", s.top1_pct),
            format!("{:.1}", s.next3_pct),
            format!("{:.2}", s.mean_chars_saved),
            format!("{:.1}", s.ms_per_token),
            format!("{:.0}", s.p50_ttft_ms),
            (s.calls - s.errors).to_string(),
        ]);
    }
    let widths: Vec<usize> = (0..7)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).chain([heads[c].len()]).max().unwrap_or(0))
        .collect();
    let line = |cells: Vec<&str>| {
        let mut out = String::from("|");
        for (c, cell) in cells.iter().enumerate() {
            if c == 0 {
                out.push_str(&format!(" {cell:<w$} |", w = widths[c]));
            } else {
                out.push_str(&format!(" {cell:>w$} |", w = widths[c]));
            }
        }
        out
    };
    println!("{}", line(heads.to_vec()));
    println!(
        "|{}|",
        widths.iter().map(|w| "-".repeat(w + 2)).collect::<Vec<_>>().join("|")
    );
    for r in &rows {
        println!("{}", line(r.iter().map(String::as_str).collect()));
    }
    for r in runs {
        if let Some(e) = &r.error {
            println!("note: {}: {}", r.name, e.lines().next().unwrap_or(""));
        }
        if r.summary.errors > 0 {
            println!("note: {}: {} of {} calls failed and are excluded", r.name, r.summary.errors, r.summary.calls);
        }
    }
}

#[derive(Serialize)]
struct Report<'a> {
    timestamp_utc: &'a str,
    llama_server_version: Option<String>,
    corpus_dir: String,
    corpus: &'a CorpusStats,
    settings: Settings,
    models: &'a [ModelRun],
}

#[derive(Serialize)]
struct Settings {
    cuts: Vec<usize>,
    n_predict: u32,
    trailing_space: bool,
    port: u16,
    calls_per_model: usize,
}

/// `(YYYYMMDD-HHMMSS, ISO-8601)` of the current UTC time.
fn utc_stamps() -> (String, String) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    (
        format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}"),
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"),
    )
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

// ---------------------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let models_dir = args.root.join("models");
    let server_exe = args.root.join("bin").join("llama-server.exe");

    if args.download {
        download::run(&models_dir).await?;
        if args.download_only {
            return Ok(());
        }
    }

    let (examples, stats) = corpus::load_examples(&args.corpus, args.sentences)?;
    let cases = build_cases(&examples, args.trailing_space);
    println!(
        "corpus {}: {} files, {} sentences kept, {} eligible ({}..{} words), {} sampled -> {} prompts per model",
        args.corpus.display(),
        stats.files,
        stats.sentences,
        stats.eligible,
        corpus::MIN_WORDS,
        corpus::MAX_WORDS,
        stats.selected,
        cases.len()
    );
    if cases.is_empty() {
        bail!("no usable sentences under {}", args.corpus.display());
    }
    if args.dry_run {
        for c in cases.iter().step_by((cases.len() / 6).max(1)) {
            println!("---\nfile {} sentence {} cut {}\nprompt: {:?}\ntruth:  {:?}", c.file, c.sentence_index, c.cut, c.prompt, c.truth);
        }
        return Ok(());
    }

    let models = if args.models.is_empty() { list_models(&models_dir)? } else { args.models.clone() };
    if models.is_empty() {
        bail!("no models: pass --model <file.gguf> or put *.gguf files in {}", models_dir.display());
    }
    for m in &models {
        if !m.is_file() {
            bail!("model not found: {}", m.display());
        }
    }
    if !server_exe.is_file() {
        bail!("llama-server not found at {}", server_exe.display());
    }

    let mut runs = Vec::new();
    for (i, model) in models.iter().enumerate() {
        println!("[{}/{}] {} ({:.2} GiB)", i + 1, models.len(), model.display(), download::gib(std::fs::metadata(model)?.len()));
        let run = run_model(&args, &server_exe, model, &cases).await;
        if let Some(e) = &run.error {
            println!("  problem: {}", e.lines().next().unwrap_or(""));
        }
        runs.push(run);
        // The sidecar was killed on drop; give the OS a moment to release the port and VRAM.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    println!();
    print_table(&runs);

    let (stamp, iso) = utc_stamps();
    std::fs::create_dir_all(&args.out_dir).with_context(|| format!("creating {}", args.out_dir.display()))?;
    let out = args.out_dir.join(format!("{stamp}.json"));
    let report = Report {
        timestamp_utc: &iso,
        llama_server_version: std::fs::read_to_string(args.root.join("bin").join("VERSION")).ok().map(|v| v.trim().to_string()),
        corpus_dir: args.corpus.display().to_string(),
        corpus: &stats,
        settings: Settings {
            cuts: CUTS.to_vec(),
            n_predict: args.n_predict,
            trailing_space: args.trailing_space,
            port: args.port,
            calls_per_model: cases.len(),
        },
        models: &runs,
    };
    std::fs::write(&out, serde_json::to_vec_pretty(&report)?).with_context(|| format!("writing {}", out.display()))?;
    println!("\nwrote {}", out.display());
    Ok(())
}

fn list_models(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("gguf")))
        .collect();
    v.sort();
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(context: &[&str]) -> Example {
        Example {
            file: "a.md".into(),
            index: 2,
            sentence: "The quick brown fox jumps over the lazy dog today.".into(),
            context: context.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn cut_needs_three_words_left() {
        // 10 words -> cut 3 (7 left) and cut 6 (4 left) fit, cut 9 (1 left) does not.
        let cases = build_cases(&[example(&[])], true);
        let cuts: Vec<usize> = cases.iter().map(|c| c.cut).collect();
        assert_eq!(cuts, [3, 6]);
    }

    #[test]
    fn prompt_and_truth_shapes() {
        let c = &build_cases(&[example(&["First one.", "Second one."])], false)[0];
        assert_eq!(c.prompt, "First one. Second one.\nThe quick brown");
        assert_eq!(c.truth, " fox jumps over the lazy dog today.");
        let c = &build_cases(&[example(&[])], true)[0];
        assert_eq!(c.prompt, "The quick brown ");
        assert_eq!(c.truth, "fox jumps over the lazy dog today.");
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_740), (2026, 10, 14));
    }
}
