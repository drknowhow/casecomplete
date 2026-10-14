//! Scoring of one completion against the true continuation, and aggregation.

use serde::Serialize;

/// Lower-cased word with leading/trailing punctuation removed.
pub fn norm_word(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// Normalised whitespace-separated words of `text`, matching how the cut points count
/// words. A token with no letters or digits (`/`, `--`) is kept as it is.
pub fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|t| {
            let w = norm_word(t);
            if w.is_empty() {
                t.to_string()
            } else {
                w
            }
        })
        .collect()
}

/// Length in characters of the longest common prefix.
pub fn lcp_chars(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Score {
    /// First generated word equals the actual next word.
    pub top1: bool,
    /// First three generated words equal the actual next three words.
    pub next3: bool,
    /// Longest common prefix of the raw completion and the true continuation, in chars.
    pub chars_saved: usize,
}

/// `completion` is the raw text the model produced; `truth` is the actual continuation of
/// the prompt (including its leading space when the prompt had no trailing space).
pub fn score(completion: &str, truth: &str) -> Score {
    let got = words(completion);
    let want = words(truth);
    Score {
        top1: matches!((got.first(), want.first()), (Some(g), Some(w)) if g == w),
        next3: want.len() >= 3 && got.len() >= 3 && got[..3] == want[..3],
        chars_saved: lcp_chars(completion, truth),
    }
}

/// Nearest-rank percentile of an unsorted slice (`p` in 0..=100); 0.0 when empty.
pub fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}

/// One measured call (also the per-example JSON row, minus the model name).
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub file: String,
    pub sentence_index: usize,
    /// Number of words typed before the cut.
    pub cut: usize,
    pub sentence: String,
    pub prompt: String,
    /// The actual continuation after the cut.
    pub truth: String,
    pub completion: String,
    pub top1: bool,
    pub next3: bool,
    pub chars_saved: usize,
    /// Streamed chunks received (one per token).
    pub n_tokens: u32,
    pub prompt_ms: f32,
    pub predicted_ms: f32,
    /// Wall time from sending the request to the first token arriving.
    pub ttft_ms: Option<f64>,
    /// Wall time of the whole call.
    pub wall_ms: f64,
    pub mean_logprob: Option<f32>,
    pub stopped_at_sentence_end: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub calls: usize,
    pub errors: usize,
    pub top1_pct: f64,
    pub next3_pct: f64,
    pub mean_chars_saved: f64,
    /// Total predicted_ms / total tokens over calls that produced tokens.
    pub ms_per_token: f64,
    pub p50_ttft_ms: f64,
    pub p95_ttft_ms: f64,
    pub mean_prompt_ms: f64,
    pub mean_wall_ms: f64,
    pub mean_tokens: f64,
}

pub fn summarize(rows: &[Row]) -> Summary {
    let ok: Vec<&Row> = rows.iter().filter(|r| r.error.is_none()).collect();
    let n = ok.len();
    let mut s = Summary { calls: rows.len(), errors: rows.len() - n, ..Default::default() };
    if n == 0 {
        return s;
    }
    let nf = n as f64;
    let pct = |count: usize| 100.0 * count as f64 / nf;
    s.top1_pct = pct(ok.iter().filter(|r| r.top1).count());
    s.next3_pct = pct(ok.iter().filter(|r| r.next3).count());
    s.mean_chars_saved = ok.iter().map(|r| r.chars_saved as f64).sum::<f64>() / nf;
    let tokens: u64 = ok.iter().map(|r| r.n_tokens as u64).sum();
    if tokens > 0 {
        let predicted: f64 = ok.iter().filter(|r| r.n_tokens > 0).map(|r| r.predicted_ms as f64).sum();
        let counted: u64 = ok.iter().filter(|r| r.n_tokens > 0).map(|r| r.n_tokens as u64).sum();
        s.ms_per_token = predicted / counted as f64;
    }
    let ttfts: Vec<f64> = ok.iter().filter_map(|r| r.ttft_ms).collect();
    s.p50_ttft_ms = percentile(&ttfts, 50.0);
    s.p95_ttft_ms = percentile(&ttfts, 95.0);
    s.mean_prompt_ms = ok.iter().map(|r| r.prompt_ms as f64).sum::<f64>() / nf;
    s.mean_wall_ms = ok.iter().map(|r| r.wall_ms).sum::<f64>() / nf;
    s.mean_tokens = tokens as f64 / nf;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_lowercased_and_stripped() {
        assert_eq!(words("  \"Hello,\" World! -- it's (fine). "), ["hello", "world", "--", "it's", "fine"]);
    }

    #[test]
    fn exact_match_scores_everything() {
        let s = score("the quick brown fox.", "the quick brown fox.");
        assert_eq!(s, Score { top1: true, next3: true, chars_saved: 20 });
    }

    #[test]
    fn top1_is_case_and_punctuation_insensitive() {
        let s = score("The, lazy dog", "the lazy cat");
        assert!(s.top1);
        assert!(!s.next3);
        assert_eq!(s.chars_saved, 0); // 'T' != 't'
    }

    #[test]
    fn chars_saved_is_a_raw_prefix_length() {
        assert_eq!(lcp_chars("the quick brown", "the quiet room"), 7);
        assert_eq!(lcp_chars("", "abc"), 0);
        assert_eq!(lcp_chars("abc", "abc and more"), 3);
    }

    #[test]
    fn empty_completion_scores_nothing() {
        assert_eq!(score("", "the quick brown fox"), Score { top1: false, next3: false, chars_saved: 0 });
    }

    #[test]
    fn leading_space_counts_when_truth_has_one() {
        assert_eq!(score(" the end.", " the end.").chars_saved, 9);
        assert_eq!(score("the end.", " the end.").chars_saved, 0);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let v = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&v, 50.0), 3.0);
        assert_eq!(percentile(&v, 95.0), 5.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }
}
