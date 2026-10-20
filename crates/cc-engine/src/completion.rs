//! Streaming client for llama-server's `/completion` endpoint.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Result of one completion request.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    /// Generated text (without the prompt). May be partial if the request was cancelled.
    pub text: String,
    /// Mean per-token log-probability, when the server reported token probabilities.
    pub mean_logprob: Option<f32>,
    /// True when generation was cut off because the text ended with `.`, `!` or `?`.
    pub stopped_at_sentence_end: bool,
    /// Server-side prompt processing time (ms); 0.0 if the server did not report it.
    pub prompt_ms: f32,
    /// Server-side generation time (ms); 0.0 if the server did not report it.
    pub predicted_ms: f32,
}

/// HTTP client bound to one llama-server instance.
#[derive(Clone)]
pub struct CompletionClient {
    http: reqwest::Client,
    base_url: String,
}

impl CompletionClient {
    pub fn new(base_url: &str) -> CompletionClient {
        // llama-server is on loopback; never route it through the system proxy.
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .tcp_nodelay(true)
            .build()
            .expect("building reqwest client");
        CompletionClient { http, base_url: base_url.trim_end_matches('/').to_string() }
    }

    /// Stream a raw-text completion of `prompt`.
    ///
    /// `on_token` receives each token's text as it arrives. Generation stops at the first
    /// newline (server-side stop string), at `n_predict` tokens, or as soon as the text
    /// ends a sentence (`.`/`!`/`?` after at least one word, optionally followed by closing
    /// quotes/brackets); in that last case `stopped_at_sentence_end` is set and the HTTP
    /// stream is dropped so the server abandons the rest.
    ///
    /// Cancelling `cancel` drops the in-flight request at once and returns `Ok` with
    /// whatever text had arrived (empty if nothing had).
    pub async fn complete(
        &self,
        prompt: &str,
        n_predict: u32,
        cancel: CancellationToken,
        mut on_token: impl FnMut(&str) + Send,
    ) -> anyhow::Result<Completion> {
        self.complete_with_probs(prompt, n_predict, cancel, |t, _| on_token(t)).await
    }

    /// Like [`complete`](Self::complete), but `on_token` also receives the token's log
    /// probability when the server reports one.
    pub async fn complete_with_probs(
        &self,
        prompt: &str,
        n_predict: u32,
        cancel: CancellationToken,
        mut on_token: impl FnMut(&str, Option<f32>) + Send,
    ) -> anyhow::Result<Completion> {
        let mut acc = Accumulator::default();
        if cancel.is_cancelled() {
            return Ok(acc.finish());
        }

        let url = format!("{}/completion", self.base_url);
        let body = request_body(prompt, n_predict);
        let resp = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(acc.finish()),
            r = self.http.post(&url).json(&body).send() => r.with_context(|| format!("POST {url}"))?,
        };

        let status = resp.status();
        if !status.is_success() {
            let text = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(acc.finish()),
                t = resp.text() => t.unwrap_or_default(),
            };
            bail!("llama-server returned HTTP {status}: {}", text.chars().take(300).collect::<String>());
        }

        let mut stream = resp.bytes_stream();
        let mut lines = LineSplitter::default();
        let mut done = false;
        'read: while !done {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => break 'read,
                c = stream.next() => c,
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.context("reading completion stream")?;
            for line in lines.push(&chunk) {
                if let Some(ev) = parse_sse_line(&line)? {
                    if acc.apply(&ev, &mut on_token) {
                        done = true;
                        break;
                    }
                }
            }
        }
        if !done && !cancel.is_cancelled() {
            if let Some(line) = lines.finish() {
                if let Some(ev) = parse_sse_line(&line)? {
                    acc.apply(&ev, &mut on_token);
                }
            }
        }
        // Leaving scope drops `stream`, closing the connection; llama-server then aborts
        // the generation for that slot and keeps the prompt in its KV cache.
        Ok(acc.finish())
    }

    /// The `n` likeliest tokens to follow `prompt`, as (token text, log probability),
    /// likeliest first. One short request that generates a single token.
    pub async fn next_tokens(&self, prompt: &str, n: u32) -> anyhow::Result<Vec<(String, f32)>> {
        let url = format!("{}/completion", self.base_url);
        let body = json!({
            "prompt": prompt,
            "n_predict": 1,
            "temperature": 0,
            "stream": false,
            "cache_prompt": true,
            "n_probs": n,
        });
        let resp = self.http.post(&url).json(&body).send().await.with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("llama-server returned HTTP {status}");
        }
        let v: Value = resp.json().await.context("reading next-token probabilities")?;
        Ok(top_tokens(&v))
    }
}

/// The candidates for the first generated token in a non-streaming `/completion` reply, in
/// any of the shapes `entry_logprob` knows.
fn top_tokens(reply: &Value) -> Vec<(String, f32)> {
    let Some(first) = reply.pointer("/completion_probabilities/0") else { return Vec::new() };
    for key in ["top_logprobs", "top_probs", "probs"] {
        let Some(list) = first.get(key).and_then(Value::as_array) else { continue };
        return list
            .iter()
            .filter_map(|c| {
                let token = c.get("token").or_else(|| c.get("tok_str")).and_then(Value::as_str)?;
                let lp = c
                    .get("logprob")
                    .and_then(Value::as_f64)
                    .filter(|x| x.is_finite())
                    .or_else(|| c.get("prob").and_then(Value::as_f64).and_then(prob_to_logprob))?;
                Some((token.to_string(), lp as f32))
            })
            .collect();
    }
    Vec::new()
}

fn request_body(prompt: &str, n_predict: u32) -> Value {
    json!({
        "prompt": prompt,
        "n_predict": n_predict,
        "temperature": 0,
        "stream": true,
        "cache_prompt": true,
        "n_probs": 1,
        // Only newline is a server-side stop: `. ! ?` must stay in the output, so sentence
        // ends are detected client-side.
        "stop": ["\n"],
        // Timings on every chunk, since we usually drop the stream before the final one.
        "timings_per_token": true,
    })
}

// ---------------------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------------------

/// One decoded `data: {...}` chunk of the completion stream.
#[derive(Debug, Default, Clone, PartialEq)]
struct SseEvent {
    content: String,
    stop: bool,
    /// Log-probability of each token carried by this chunk (usually one).
    logprobs: Vec<f64>,
    prompt_ms: Option<f32>,
    predicted_ms: Option<f32>,
}

/// Splits a byte stream into lines, tolerating chunk boundaries inside a line or inside a
/// multi-byte UTF-8 sequence.
#[derive(Default)]
struct LineSplitter {
    buf: Vec<u8>,
}

fn decode_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end_matches('\r').to_string()
}

impl LineSplitter {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            out.push(decode_line(&line[..pos]));
        }
        out
    }

    /// The unterminated trailing line, if the stream ended without a final newline.
    fn finish(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let line = decode_line(&std::mem::take(&mut self.buf));
        Some(line)
    }
}

/// Parse one SSE line. Non-data lines (blank, comments, `event:`), `[DONE]` and
/// undecodable JSON yield `Ok(None)`; a server-reported error yields `Err`.
fn parse_sse_line(line: &str) -> Result<Option<SseEvent>> {
    let line = line.trim_end_matches('\r');
    let payload = if let Some(p) = line.strip_prefix("data:") {
        p.trim_start()
    } else if let Some(p) = line.strip_prefix("error:") {
        bail!("llama-server error: {}", p.trim());
    } else {
        return Ok(None);
    };
    if payload.is_empty() || payload == "[DONE]" {
        return Ok(None);
    }
    let v: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("ignoring undecodable completion chunk ({e}): {payload}");
            return Ok(None);
        }
    };
    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| err.to_string());
        return Err(anyhow!("llama-server error: {msg}"));
    }

    let timing = |key: &str| v.pointer(&format!("/timings/{key}")).and_then(Value::as_f64).map(|x| x as f32);
    Ok(Some(SseEvent {
        content: v.get("content").and_then(Value::as_str).unwrap_or_default().to_string(),
        stop: v.get("stop").and_then(Value::as_bool).unwrap_or(false),
        logprobs: extract_logprobs(&v),
        prompt_ms: timing("prompt_ms"),
        predicted_ms: timing("predicted_ms"),
    }))
}

fn prob_to_logprob(p: f64) -> Option<f64> {
    (p.is_finite() && p > 0.0).then(|| p.ln())
}

/// Log-probability of the chosen token from one `completion_probabilities` entry.
///
/// Shapes seen across llama.cpp versions:
/// - current: `{"token", "logprob", "top_logprobs": [{"token", "logprob"}]}`
/// - post-sampling: `{"token", "prob", "top_probs": [{"token", "prob"}]}`
/// - older: `{"content", "probs": [{"tok_str", "prob"}]}`
///
/// Sampling is greedy (`temperature: 0`), so the first candidate is the chosen token.
fn entry_logprob(entry: &Value) -> Option<f64> {
    if let Some(lp) = entry.get("logprob").and_then(Value::as_f64).filter(|x| x.is_finite()) {
        return Some(lp);
    }
    if let Some(lp) = entry.get("prob").and_then(Value::as_f64).and_then(prob_to_logprob) {
        return Some(lp);
    }
    for key in ["top_logprobs", "top_probs", "probs"] {
        let Some(first) = entry.get(key).and_then(Value::as_array).and_then(|a| a.first()) else {
            continue;
        };
        if let Some(lp) = first.get("logprob").and_then(Value::as_f64).filter(|x| x.is_finite()) {
            return Some(lp);
        }
        if let Some(lp) = first.get("prob").and_then(Value::as_f64).and_then(prob_to_logprob) {
            return Some(lp);
        }
    }
    None
}

fn extract_logprobs(chunk: &Value) -> Vec<f64> {
    chunk
        .get("completion_probabilities")
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(entry_logprob).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Sentence-end detection + accumulation
// ---------------------------------------------------------------------------------------

/// True if `text` ends a sentence: its last character, ignoring trailing closing quotes
/// and brackets, is `.`, `!` or `?`, and at least one word precedes it.
fn ends_sentence(text: &str) -> bool {
    let trimmed = text.trim_end_matches(|c: char| {
        matches!(c, '"' | '\'' | ')' | ']' | '}' | '\u{201d}' | '\u{2019}' | '\u{00bb}')
    });
    let mut rest = trimmed.chars();
    match rest.next_back() {
        Some('.' | '!' | '?') => rest.as_str().chars().any(char::is_alphanumeric),
        _ => false,
    }
}

#[derive(Default)]
struct Accumulator {
    text: String,
    logprob_sum: f64,
    logprob_count: u32,
    prompt_ms: f32,
    predicted_ms: f32,
    sentence_end: bool,
}

impl Accumulator {
    /// Fold one chunk in. Returns true when the request is finished (final chunk or
    /// sentence end) and the stream should be dropped.
    fn apply(&mut self, ev: &SseEvent, on_token: &mut dyn FnMut(&str, Option<f32>)) -> bool {
        if let Some(ms) = ev.prompt_ms {
            self.prompt_ms = ms;
        }
        if let Some(ms) = ev.predicted_ms {
            self.predicted_ms = ms;
        }
        for lp in &ev.logprobs {
            self.logprob_sum += lp;
            self.logprob_count += 1;
        }
        if !ev.content.is_empty() {
            self.text.push_str(&ev.content);
            let chunk_logprob = (!ev.logprobs.is_empty())
                .then(|| (ev.logprobs.iter().sum::<f64>() / ev.logprobs.len() as f64) as f32);
            on_token(&ev.content, chunk_logprob);
            if ends_sentence(&self.text) {
                self.sentence_end = true;
                return true;
            }
        }
        ev.stop
    }

    fn finish(self) -> Completion {
        Completion {
            mean_logprob: (self.logprob_count > 0).then(|| (self.logprob_sum / self.logprob_count as f64) as f32),
            text: self.text,
            stopped_at_sentence_end: self.sentence_end,
            prompt_ms: self.prompt_ms,
            predicted_ms: self.predicted_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn ev(line: &str) -> SseEvent {
        parse_sse_line(line).unwrap().expect("expected an event")
    }

    // ---- sentence-end detection ----

    #[test]
    fn sentence_end_basic() {
        assert!(ends_sentence("It works."));
        assert!(ends_sentence("Really?"));
        assert!(ends_sentence("Stop!"));
        assert!(ends_sentence(" over the lazy dog."));
    }

    #[test]
    fn sentence_end_needs_a_word() {
        assert!(!ends_sentence("."));
        assert!(!ends_sentence(" ."));
        assert!(!ends_sentence("?!"));
        assert!(!ends_sentence("...") );
        assert!(!ends_sentence(""));
    }

    #[test]
    fn sentence_end_allows_closing_quotes_and_brackets() {
        assert!(ends_sentence("He said \"go home.\""));
        assert!(ends_sentence("(see above.)"));
        assert!(ends_sentence("it's fine.'"));
        assert!(ends_sentence("\u{201c}Fine.\u{201d}"));
        assert!(ends_sentence("done.)\""));
    }

    #[test]
    fn mid_sentence_is_not_an_end() {
        assert!(!ends_sentence("the quick brown"));
        assert!(!ends_sentence("Hello, "));
        // trailing whitespace after the terminator means the stream already moved on
        assert!(!ends_sentence("Done. "));
        assert!(!ends_sentence("and then \""));
    }

    // ---- SSE parsing ----

    const NEW_SHAPE: &str = r#"data: {"index":0,"content":" world","tokens":[1879],"stop":false,"id_slot":0,"completion_probabilities":[{"id":1879,"logprob":-0.25,"token":" world","bytes":[32],"top_logprobs":[{"id":1879,"logprob":-0.25,"token":" world","bytes":[32]}]}]}"#;
    const NEW_SHAPE_TOP_ONLY: &str = r#"data: {"content":"!","stop":false,"completion_probabilities":[{"id":1,"token":"!","top_logprobs":[{"id":1,"logprob":-1.5,"token":"!"}]}]}"#;
    const POST_SAMPLING: &str = r#"data: {"content":"a","stop":false,"completion_probabilities":[{"id":5,"token":"a","prob":0.5,"top_probs":[{"id":5,"token":"a","prob":0.5}]}]}"#;
    const OLD_SHAPE: &str = r#"data: {"content":" the","stop":false,"completion_probabilities":[{"content":" the","probs":[{"tok_str":" the","prob":0.25}]}]}"#;
    const FINAL: &str = r#"data: {"index":0,"content":"","tokens":[],"stop":true,"stop_type":"limit","tokens_predicted":24,"timings":{"prompt_n":7,"prompt_ms":12.5,"predicted_n":24,"predicted_ms":180.25,"predicted_per_second":133.1}}"#;

    #[test]
    fn parses_current_logprob_shape() {
        let e = ev(NEW_SHAPE);
        assert_eq!(e.content, " world");
        assert!(!e.stop);
        assert_eq!(e.logprobs, vec![-0.25]);
        assert_eq!(e.prompt_ms, None);
    }

    #[test]
    fn parses_top_logprobs_only() {
        assert_eq!(ev(NEW_SHAPE_TOP_ONLY).logprobs, vec![-1.5]);
    }

    #[test]
    fn parses_post_sampling_prob() {
        let lp = ev(POST_SAMPLING).logprobs;
        assert_eq!(lp.len(), 1);
        assert!((lp[0] - 0.5f64.ln()).abs() < 1e-12);
    }

    #[test]
    fn parses_legacy_probs_shape() {
        let lp = ev(OLD_SHAPE).logprobs;
        assert_eq!(lp.len(), 1);
        assert!((lp[0] - 0.25f64.ln()).abs() < 1e-12);
    }

    #[test]
    fn no_probabilities_means_empty() {
        let e = ev(r#"data: {"content":"x","stop":false}"#);
        assert!(e.logprobs.is_empty());
        let e = ev(r#"data: {"content":"x","stop":false,"completion_probabilities":[{"token":"x"}]}"#);
        assert!(e.logprobs.is_empty());
    }

    #[test]
    fn top_tokens_reads_every_reply_shape() {
        let current: Value = serde_json::from_str(
            r#"{"content":" month","completion_probabilities":[{"token":" month","logprob":-0.5,
                "top_logprobs":[{"id":1,"token":" month","logprob":-0.5},{"id":2,"token":" week","logprob":-1.5}]}]}"#,
        )
        .unwrap();
        assert_eq!(top_tokens(&current), vec![(" month".to_string(), -0.5), (" week".to_string(), -1.5)]);
        let post: Value = serde_json::from_str(
            r#"{"completion_probabilities":[{"token":" a","prob":0.5,"top_probs":[{"token":" a","prob":0.5},{"token":" b","prob":0.0}]}]}"#,
        )
        .unwrap();
        let t = top_tokens(&post);
        assert_eq!(t.len(), 1, "a zero probability has no logprob: {t:?}");
        assert!((t[0].1 - 0.5f32.ln()).abs() < 1e-6);
        let old: Value =
            serde_json::from_str(r#"{"completion_probabilities":[{"content":" x","probs":[{"tok_str":" x","prob":1.0}]}]}"#)
                .unwrap();
        assert_eq!(top_tokens(&old), vec![(" x".to_string(), 0.0)]);
        assert!(top_tokens(&json!({"content": ""})).is_empty());
    }

    #[test]
    fn parses_final_chunk_timings() {
        let e = ev(FINAL);
        assert!(e.stop);
        assert!(e.content.is_empty());
        assert_eq!(e.prompt_ms, Some(12.5));
        assert_eq!(e.predicted_ms, Some(180.25));
    }

    #[test]
    fn ignores_non_data_lines() {
        for l in ["", ": keep-alive", "event: message", "id: 3", "data: [DONE]", "data:", "data: {not json"] {
            assert_eq!(parse_sse_line(l).unwrap(), None, "line {l:?}");
        }
    }

    #[test]
    fn tolerates_crlf_and_missing_space() {
        let e = ev("data:{\"content\":\"hi\",\"stop\":false}\r");
        assert_eq!(e.content, "hi");
    }

    #[test]
    fn error_events_are_errors() {
        let err = parse_sse_line(r#"data: {"error":{"code":500,"message":"slot busy","type":"server_error"}}"#)
            .unwrap_err();
        assert!(err.to_string().contains("slot busy"), "{err}");
        assert!(parse_sse_line("error: {\"message\":\"boom\"}").is_err());
    }

    #[test]
    fn line_splitter_handles_split_lines_and_utf8() {
        let full = "data: {\"content\":\"caf\u{e9}\"}\n\ndata: {\"content\":\"b\"}\n";
        let bytes = full.as_bytes();
        // Cut inside the two-byte sequence of the e-acute.
        let cut = full.find('\u{e9}').unwrap() + 1;
        let mut s = LineSplitter::default();
        assert!(s.push(&bytes[..cut]).is_empty());
        let lines = s.push(&bytes[cut..]);
        assert_eq!(lines, vec!["data: {\"content\":\"caf\u{e9}\"}", "", "data: {\"content\":\"b\"}"]);
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn line_splitter_flushes_unterminated_tail() {
        let mut s = LineSplitter::default();
        assert!(s.push(b"data: {\"content\":\"x\"}").is_empty());
        assert_eq!(s.finish().as_deref(), Some("data: {\"content\":\"x\"}"));
    }

    // ---- accumulation over a whole stream ----

    fn run(lines: &[&str]) -> (Completion, Vec<String>) {
        let mut acc = Accumulator::default();
        let mut tokens = Vec::new();
        for l in lines {
            if let Some(e) = parse_sse_line(l).unwrap() {
                if acc.apply(&e, &mut |t: &str, _: Option<f32>| tokens.push(t.to_string())) {
                    break;
                }
            }
        }
        (acc.finish(), tokens)
    }

    fn tok(content: &str, logprob: f64) -> String {
        format!(
            r#"data: {{"content":{},"stop":false,"completion_probabilities":[{{"token":{},"logprob":{logprob}}}]}}"#,
            serde_json::to_string(content).unwrap(),
            serde_json::to_string(content).unwrap()
        )
    }

    #[test]
    fn stops_at_sentence_end_and_averages_logprobs() {
        let lines = [
            tok(" quick", -1.0),
            tok(" fox", -3.0),
            tok(".", -2.0),
            tok(" Next", -9.0),
            FINAL.to_string(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (c, tokens) = run(&refs);
        assert_eq!(c.text, " quick fox.");
        assert_eq!(tokens, vec![" quick", " fox", "."]);
        assert!(c.stopped_at_sentence_end);
        assert!((c.mean_logprob.unwrap() - (-2.0)).abs() < 1e-6);
    }

    #[test]
    fn runs_to_final_chunk_and_reports_timings() {
        let lines = [tok(" and", -0.5), tok(" then", -1.5), FINAL.to_string()];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (c, _) = run(&refs);
        assert_eq!(c.text, " and then");
        assert!(!c.stopped_at_sentence_end);
        assert_eq!(c.prompt_ms, 12.5);
        assert_eq!(c.predicted_ms, 180.25);
        assert!((c.mean_logprob.unwrap() + 1.0).abs() < 1e-6);
    }

    #[test]
    fn timings_per_token_survive_early_stop() {
        let with_timing = r#"data: {"content":".","stop":false,"timings":{"prompt_ms":3.5,"predicted_ms":41.0}}"#;
        let lines = [r#"data: {"content":"Hi","stop":false,"timings":{"prompt_ms":3.5,"predicted_ms":20.0}}"#, with_timing];
        let (c, _) = run(&lines);
        assert!(c.stopped_at_sentence_end);
        assert_eq!(c.prompt_ms, 3.5);
        assert_eq!(c.predicted_ms, 41.0);
    }

    #[test]
    fn no_logprobs_gives_none() {
        let lines = [r#"data: {"content":"Hi","stop":false}"#, r#"data: {"content":"","stop":true}"#];
        let (c, _) = run(&lines);
        assert_eq!(c.mean_logprob, None);
        assert_eq!(c.text, "Hi");
    }

    #[test]
    fn leading_terminator_without_word_does_not_stop() {
        let lines = [tok(".", -0.1), tok(" Yes", -0.2), tok(".", -0.3)];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (c, _) = run(&refs);
        assert_eq!(c.text, ". Yes.");
        assert!(c.stopped_at_sentence_end);
    }

    #[test]
    fn request_body_shape() {
        let b = request_body("abc", 24);
        assert_eq!(b["prompt"], "abc");
        assert_eq!(b["n_predict"], 24);
        assert_eq!(b["stream"], true);
        assert_eq!(b["cache_prompt"], true);
        assert_eq!(b["n_probs"], 1);
        assert_eq!(b["temperature"], 0);
        assert_eq!(b["stop"], json!(["\n"]));
    }

    // ---- end to end against a loopback mock server ----

    /// Accepts one connection, returns the request body, then replays `events`
    /// (delay before, raw bytes) and optionally holds the connection open.
    fn spawn_mock(
        events: Vec<(Duration, String)>,
        hold_open: Duration,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let listener = TcpListener::from_std(std_listener).unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let body = read_request(&mut sock).await;
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = sock.write_all(head.as_bytes()).await;
            for (delay, chunk) in events {
                tokio::time::sleep(delay).await;
                if sock.write_all(chunk.as_bytes()).await.is_err() || sock.flush().await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(hold_open).await;
            body
        });
        (format!("http://{addr}"), handle)
    }

    async fn read_request(sock: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = sock.read(&mut tmp).await.unwrap();
            assert!(n > 0, "client closed before sending a request");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                while buf.len() < end + 4 + len {
                    let n = sock.read(&mut tmp).await.unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&tmp[..n]);
                }
                return String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned();
            }
        }
    }

    fn sse(line: &str) -> (Duration, String) {
        (Duration::from_millis(5), format!("{line}\n\n"))
    }

    #[tokio::test]
    async fn complete_stops_at_sentence_end() {
        let events = vec![
            sse(&tok(" jumps", -1.0)),
            sse(&tok(" over", -1.0)),
            sse(&tok(".", -1.0)),
            sse(&tok(" never seen", -1.0)),
            sse(FINAL),
        ];
        let (base, server) = spawn_mock(events, Duration::from_millis(50));
        let client = CompletionClient::new(&format!("{base}/"));
        let mut seen = Vec::new();
        let c = client
            .complete("The fox", 24, CancellationToken::new(), |t| seen.push(t.to_string()))
            .await
            .unwrap();
        assert_eq!(c.text, " jumps over.");
        assert_eq!(seen, vec![" jumps", " over", "."]);
        assert!(c.stopped_at_sentence_end);
        assert!((c.mean_logprob.unwrap() + 1.0).abs() < 1e-6);

        let body: Value = serde_json::from_str(&server.await.unwrap()).unwrap();
        assert_eq!(body["prompt"], "The fox");
        assert_eq!(body["n_predict"], 24);
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn complete_reads_to_final_chunk() {
        let events = vec![sse(&tok(" a", -1.0)), sse(&tok(" b", -1.0)), sse(FINAL)];
        let (base, _server) = spawn_mock(events, Duration::ZERO);
        let c = CompletionClient::new(&base)
            .complete("x", 2, CancellationToken::new(), |_| {})
            .await
            .unwrap();
        assert_eq!(c.text, " a b");
        assert!(!c.stopped_at_sentence_end);
        assert_eq!(c.prompt_ms, 12.5);
        assert_eq!(c.predicted_ms, 180.25);
    }

    #[tokio::test]
    async fn cancel_drops_in_flight_request_immediately() {
        let events = vec![sse(&tok(" Hello", -1.0))];
        // The server then goes silent for 10 s; only cancellation can end the call.
        let (base, _server) = spawn_mock(events, Duration::from_secs(10));
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let c = tokio::time::timeout(
            Duration::from_secs(3),
            CompletionClient::new(&base).complete("x", 24, cancel, |_| {}),
        )
        .await
        .expect("complete() must return promptly after cancel")
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(c.text, " Hello");
        assert!(!c.stopped_at_sentence_end);
    }

    #[tokio::test]
    async fn already_cancelled_makes_no_request() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        // Nothing listens on this port; a request would error.
        let c = CompletionClient::new("http://127.0.0.1:9")
            .complete("x", 24, cancel, |_| {})
            .await
            .unwrap();
        assert!(c.text.is_empty());
    }

    #[tokio::test]
    async fn server_error_status_is_an_error() {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let listener = TcpListener::from_std(std_listener).unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            read_request(&mut sock).await;
            let body = r#"{"error":{"message":"model busy"}}"#;
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        let err = CompletionClient::new(&format!("http://{addr}"))
            .complete("x", 4, CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("503"), "{err}");
        assert!(err.to_string().contains("model busy"), "{err}");
    }
}
