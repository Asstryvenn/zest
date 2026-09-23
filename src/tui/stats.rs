//! Token accounting and the one-line terminal readout.

use serde::Serialize;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

fn bpe() -> &'static CoreBPE {
    static BPE: OnceLock<CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::o200k_base().expect("embedded o200k_base vocabulary"))
}

/// Token count using the o200k BPE. Provider tokenizers differ (Claude's runs
/// roughly 1-1.35x higher), so treat this as an estimate that is consistent
/// between the before and after numbers.
pub fn count_tokens(s: &str) -> usize {
    bpe().encode_ordinary(s).len()
}

/// Warm the tokenizer so the first request doesn't pay for vocabulary loading.
pub fn warm_up() {
    let _ = bpe();
}

/// Input list price in USD per million tokens, by model-name prefix.
pub fn input_price_per_mtok(model: Option<&str>, default: f64) -> f64 {
    let Some(m) = model else { return default };
    const TABLE: &[(&str, f64)] = &[
        ("claude-fable-5", 10.0),
        ("claude-mythos-5", 10.0),
        ("claude-opus-5-5", 4.0),
        ("claude-opus-5", 5.0),
        ("claude-opus-4-8", 5.0),
        ("claude-opus-4-7", 5.0),
        ("claude-opus-4-6", 5.0),
        ("claude-opus-4-5", 5.0),
        ("claude-sonnet-5", 2.0),
        ("claude-sonnet-4", 3.0),
        ("claude-haiku-4-5", 1.0),
    ];
    TABLE.iter().find(|(p, _)| m.starts_with(p)).map_or(default, |(_, price)| *price)
}

#[derive(Default)]
pub struct Stats {
    requests: AtomicU64,
    tokens_in: AtomicU64,
    tokens_out: AtomicU64,
    saved_nano_usd: AtomicU64,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub requests: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_saved: u64,
    pub ratio_saved: f64,
    pub usd_saved: f64,
}

impl Stats {
    pub fn record(&self, before: usize, after: usize, usd_saved: f64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.tokens_in.fetch_add(before as u64, Ordering::Relaxed);
        self.tokens_out.fetch_add(after as u64, Ordering::Relaxed);
        self.saved_nano_usd.fetch_add((usd_saved * 1e9) as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        let tin = self.tokens_in.load(Ordering::Relaxed);
        let tout = self.tokens_out.load(Ordering::Relaxed);
        Snapshot {
            requests: self.requests.load(Ordering::Relaxed),
            tokens_in: tin,
            tokens_out: tout,
            tokens_saved: tin.saturating_sub(tout),
            ratio_saved: if tin == 0 { 0.0 } else { 1.0 - tout as f64 / tin as f64 },
            usd_saved: self.saved_nano_usd.load(Ordering::Relaxed) as f64 / 1e9,
        }
    }

    pub fn summary_line(&self) -> String {
        let s = self.snapshot();
        format!(
            "[TokSqueeze] session: {} requests | {} -> {} tokens ({}) | Saved: ~${:.3}",
            s.requests,
            thousands(s.tokens_in as usize),
            thousands(s.tokens_out as usize),
            pct(s.tokens_in as usize, s.tokens_out as usize),
            s.usd_saved
        )
    }
}

pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn pct(before: usize, after: usize) -> String {
    if before == 0 {
        return "-0.0%".into();
    }
    format!("{:+.1}%", (after as f64 / before as f64 - 1.0) * 100.0)
}

/// `[TokSqueeze] 18,400 -> 3,100 tokens (-83.1%) | Saved: ~$0.045 | Latency: 3.2ms`
pub fn format_line(before: usize, after: usize, usd_saved: f64, latency_ms: f64, suffix: &str) -> String {
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let (g, d, r) = if color { ("\x1b[32m", "\x1b[2m", "\x1b[0m") } else { ("", "", "") };
    let mut line = format!(
        "[TokSqueeze] {} -> {} tokens ({g}{}{r}) | Saved: ~${:.3} | Latency: {:.1}ms",
        thousands(before),
        thousands(after),
        pct(before, after),
        usd_saved,
        latency_ms
    );
    if !suffix.is_empty() {
        line.push_str(&format!(" {d}{suffix}{r}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_the_spec() {
        std::env::set_var("NO_COLOR", "1");
        assert_eq!(
            format_line(18400, 3100, 0.045, 3.2, ""),
            "[TokSqueeze] 18,400 -> 3,100 tokens (-83.2%) | Saved: ~$0.045 | Latency: 3.2ms"
        );
    }

    #[test]
    fn prices_by_prefix() {
        assert_eq!(input_price_per_mtok(Some("claude-opus-5-5"), 3.0), 4.0);
        assert_eq!(input_price_per_mtok(Some("claude-opus-5"), 3.0), 5.0);
        assert_eq!(input_price_per_mtok(Some("gpt-5"), 3.0), 3.0);
    }
}
