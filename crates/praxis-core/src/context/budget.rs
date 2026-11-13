//! Token budget, calibrated estimates and head-and-tail truncation (SPEC §7).

use std::io;
use std::path::PathBuf;

/// The starting estimate before the server has reported anything (SPEC §7).
const DEFAULT_CHARS_PER_TOKEN: f64 = 3.5;
/// Half the error goes per sample: fast to settle, and one odd prompt can't swing it.
const CALIBRATION_WEIGHT: f64 = 0.5;
/// A judgement, not a measurement: estimates were within 2% in T0.3.
const SHRUNK_PROMPT_PERCENT: u64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    Calibrated,
    /// The server saw far less than was sent, so it may have dropped part of the prompt.
    PromptShrank {
        reported: u32,
        estimated: u32,
    },
}

/// How one session's context window `C` is shared out.
#[derive(Debug, Clone)]
pub struct Budget {
    context_size: u32,
    chars_per_token: f64,
}

impl Budget {
    pub fn new(context_size: u32) -> Self {
        Self {
            context_size,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        }
    }

    /// `percent` of `C`.
    pub fn share(&self, percent: u64) -> u32 {
        // Never fails: callers pass at most 100, so the result fits the u32 context size.
        u32::try_from(u64::from(self.context_size) * percent / 100).unwrap_or(self.context_size)
    }

    pub fn output_reserve(&self) -> u32 {
        self.share(15)
    }

    pub fn prefix_limit(&self) -> u32 {
        self.share(25)
    }

    pub fn result_limit(&self) -> u32 {
        self.share(10)
    }

    pub fn compact_at(&self) -> u32 {
        self.share(80)
    }

    /// Masking frees enough in one rewrite to stay under `compact_at` for a good while.
    pub fn mask_target(&self) -> u32 {
        self.share(60)
    }

    /// At or below this, masking was enough and no summary is needed.
    pub fn settle_at(&self) -> u32 {
        self.share(70)
    }

    pub fn estimate(&self, chars: usize) -> u32 {
        (chars as f64 / self.chars_per_token).ceil() as u32
    }

    pub fn chars_for(&self, tokens: u32) -> usize {
        (f64::from(tokens) * self.chars_per_token) as usize
    }

    /// Learns from what the server counted for a prompt of `sent_chars`. A suspiciously small
    /// count is left out: it would teach the estimate to trust a truncated prompt.
    pub fn observe(&mut self, sent_chars: usize, reported: u32) -> Observation {
        let estimated = self.estimate(sent_chars);
        if u64::from(reported) * 100 < u64::from(estimated) * SHRUNK_PROMPT_PERCENT {
            return Observation::PromptShrank {
                reported,
                estimated,
            };
        }
        if reported > 0 {
            let measured = sent_chars as f64 / f64::from(reported);
            self.chars_per_token += (measured - self.chars_per_token) * CALIBRATION_WEIGHT;
        }
        Observation::Calibrated
    }

    /// Keeps a result within its limit: head and tail stay, and the full text goes to `spool`.
    /// Applied once, when the result is first recorded, so history is never rewritten for it.
    pub fn fit_result(
        &self,
        content: String,
        spool: impl FnOnce(&str) -> io::Result<PathBuf>,
    ) -> String {
        let total = content.chars().count();
        if self.estimate(total) <= self.result_limit() {
            return content;
        }
        // shortcut: Read pages by line, so the middle of one giant line stays out of reach;
        // add byte ranges to Read if real output needs them.
        let where_full = match spool(&content) {
            Ok(path) => format!(
                "The full output is in {} and Read can page through it with offset and limit.",
                path.display()
            ),
            Err(error) => {
                tracing::warn!(%error, "couldn't spool an oversized tool result");
                "The full output couldn't be saved.".to_owned()
            }
        };
        let notice = format!("\n[Output truncated: {total} characters in all. {where_full}]\n");
        let kept = self
            .chars_for(self.result_limit())
            .saturating_sub(notice.chars().count())
            .min(total);
        let head: String = content.chars().take(kept / 2).collect();
        let tail: String = content.chars().skip(total - (kept - kept / 2)).collect();
        format!("{head}{notice}{tail}")
    }

    /// Fits memory files, broadest first, into what the prefix limit leaves after `other_chars`.
    /// The nearest files are kept first. The first one that doesn't fit is cut from its end, and
    /// every broader one is dropped. Returns the kept files in order and whether anything was cut.
    pub fn fit_memory(&self, memory: &[String], other_chars: usize) -> (Vec<String>, bool) {
        let mut room = self
            .chars_for(self.prefix_limit())
            .saturating_sub(other_chars);
        let mut kept = Vec::new();
        for file in memory.iter().rev() {
            let len = file.chars().count();
            if len <= room {
                room -= len;
                kept.push(file.clone());
                continue;
            }
            let piece: String = file.chars().take(room).collect();
            if !piece.is_empty() {
                kept.push(piece);
            }
            kept.reverse();
            return (kept, true);
        }
        kept.reverse();
        (kept, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::path::Path;

    fn no_spool(_: &str) -> io::Result<PathBuf> {
        panic!("nothing should be spooled")
    }

    /// `(prompt chars, reported prompt tokens)` from a T0.3 recording, untouched.
    fn recorded(model: &str, case: &str) -> (usize, u32) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lmstudio")
            .join(model);
        let request = fs::read_to_string(dir.join(format!("{case}.request.json"))).unwrap();
        let request: Value = serde_json::from_str(&request).unwrap();
        let content = request["messages"][0]["content"].as_str().unwrap();
        let chars = content
            .strip_prefix('<')
            .and_then(|c| c.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap();
        let sse = fs::read_to_string(dir.join(format!("{case}.sse"))).unwrap();
        let tokens = sse
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .find_map(|v| v["usage"]["prompt_tokens"].as_u64())
            .unwrap();
        (chars, u32::try_from(tokens).unwrap())
    }

    #[test]
    fn category_limits_are_the_specified_shares_of_the_context() {
        let budget = Budget::new(8192);
        assert_eq!(budget.output_reserve(), 1228);
        assert_eq!(budget.prefix_limit(), 2048);
        assert_eq!(budget.result_limit(), 819);
        assert_eq!(Budget::new(u32::MAX).output_reserve(), 644_245_094);
    }

    #[test]
    fn starts_from_three_and_a_half_chars_per_token() {
        assert_eq!(Budget::new(8192).estimate(35), 10);
    }

    #[test]
    fn calibration_converges_on_recorded_usage() {
        for model in ["gemma-4-12b-qat", "qwen3.5-4b", "qwen2.5-coder-7b-instruct"] {
            let (chars, tokens) = recorded(model, "oversize_0.5");
            let (other_chars, other_tokens) = recorded(model, "oversize_0.9");
            let mut budget = Budget::new(8192);
            let before = budget.estimate(other_chars).abs_diff(other_tokens);
            for _ in 0..6 {
                assert_eq!(budget.observe(chars, tokens), Observation::Calibrated);
            }
            let off = budget.estimate(chars).abs_diff(tokens);
            assert!(off * 50 <= tokens, "{model}: off by {off} of {tokens}");
            // A prompt it never saw lands close as well, and closer than the default did.
            let after = budget.estimate(other_chars).abs_diff(other_tokens);
            assert!(after * 20 <= other_tokens, "{model}: off by {after}");
            assert!(after < before, "{model}");
        }
    }

    #[test]
    fn a_shrunken_prompt_warns_and_does_not_teach_the_estimate() {
        let mut budget = Budget::new(8192);
        let estimated = budget.estimate(7000);
        assert_eq!(
            budget.observe(7000, 900),
            Observation::PromptShrank {
                reported: 900,
                estimated: 2000
            }
        );
        assert_eq!(budget.estimate(7000), estimated);
        assert_eq!(budget.observe(7000, 1000), Observation::Calibrated);
    }

    #[test]
    fn a_result_within_its_limit_is_untouched() {
        let budget = Budget::new(1000);
        let fits = "x".repeat(350);
        assert_eq!(budget.fit_result(fits.clone(), no_spool), fits);
    }

    #[test]
    fn truncation_keeps_head_and_tail_and_splits_no_character() {
        let budget = Budget::new(1000);
        let content = format!("{}{}{}", "é".repeat(200), "m".repeat(9000), "ü".repeat(200));
        let fitted = budget.fit_result(content.clone(), |full| {
            assert_eq!(full, content);
            Ok(PathBuf::from("/spool/result-1.txt"))
        });
        assert!(fitted.chars().count() <= 350);
        assert!(fitted.starts_with('é') && fitted.ends_with('ü'));
        assert!(fitted.contains("/spool/result-1.txt"));
    }

    #[test]
    fn a_failed_spool_still_truncates_and_says_so() {
        let budget = Budget::new(1000);
        let fitted = budget.fit_result("x".repeat(9000), |_| Err(io::ErrorKind::Other.into()));
        assert!(fitted.chars().count() <= 350);
        assert!(fitted.contains("couldn't be saved"));
    }

    #[test]
    fn memory_is_cut_only_when_over_what_is_left() {
        let budget = Budget::new(1000);
        assert_eq!(
            budget.fit_memory(&["short".to_owned()], 0),
            (vec!["short".to_owned()], false)
        );
        let (kept, cut) = budget.fit_memory(&["é".repeat(2000)], 500);
        assert_eq!((kept[0].chars().count(), cut), (375, true));
    }

    #[test]
    fn the_broadest_memory_is_trimmed_first() {
        let budget = Budget::new(1000);
        let (kept, cut) = budget.fit_memory(&["B".repeat(700), "N".repeat(300)], 0);
        assert_eq!(kept, ["B".repeat(575), "N".repeat(300)]);
        assert!(cut);
        let (kept, cut) = budget.fit_memory(&["B".repeat(100), "N".repeat(900)], 0);
        assert_eq!(kept, ["N".repeat(875)]);
        assert!(cut);
    }
}
