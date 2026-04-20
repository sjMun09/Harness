//! Token counting + budget tracking. PLAN §3.1 (tiktoken-rs + 0.9 safety factor).

#![forbid(unsafe_code)]

use std::sync::{Arc, OnceLock};

use harness_proto::Usage;
use thiserror::Error;
use tiktoken_rs::{cl100k_base, CoreBPE};

/// Safety factor applied to the budget cap — model tokenizers drift from our
/// local `cl100k_base` estimate, so we trip `exceeded()` early. PLAN §6.
pub const BUDGET_SAFETY_FACTOR: f64 = 0.9;

#[derive(Debug, Clone, Copy, Default)]
pub struct Budget {
    pub max_total_tokens: u64,
    pub used: Usage,
}

impl Budget {
    pub fn new(max_total_tokens: u64) -> Self {
        Self {
            max_total_tokens,
            used: Usage::default(),
        }
    }

    pub fn add(&mut self, usage: Usage) {
        self.used = self.used.merge(usage);
    }

    pub fn exceeded(&self) -> bool {
        if self.max_total_tokens == 0 {
            return false;
        }
        let total = self
            .used
            .input_tokens
            .saturating_add(self.used.output_tokens);
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cap = ((self.max_total_tokens as f64) * BUDGET_SAFETY_FACTOR) as u64;
        total >= cap
    }
}

/// Estimator trait — real impl wraps `tiktoken_rs::cl100k_base` behind `OnceLock`.
pub trait TokenEstimator: Send + Sync {
    fn count(&self, text: &str) -> usize;
}

/// Coarse `len / 4` heuristic — used in tests and when BPE init fails (e.g. offline).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullEstimator;

impl TokenEstimator for NullEstimator {
    fn count(&self, text: &str) -> usize {
        // cl100k_base averages ~3.5-4 bytes/token on English. Keep coarse.
        text.len() / 4
    }
}

/// Lazy, process-wide cl100k_base BPE. First call may cost ~5-20ms on disk-backed
/// cache warm-up; subsequent calls are O(1) lookups.
#[derive(Debug, Default, Clone, Copy)]
pub struct TiktokenEstimator;

impl TokenEstimator for TiktokenEstimator {
    fn count(&self, text: &str) -> usize {
        match cl100k_bpe() {
            Ok(bpe) => bpe.encode_with_special_tokens(text).len(),
            Err(_) => NullEstimator.count(text),
        }
    }
}

fn cl100k_bpe() -> Result<&'static CoreBPE, TokenError> {
    static BPE: OnceLock<Result<Arc<CoreBPE>, String>> = OnceLock::new();
    let cell = BPE.get_or_init(|| cl100k_base().map(Arc::new).map_err(|e| e.to_string()));
    match cell {
        Ok(arc) => Ok(arc.as_ref()),
        Err(e) => Err(TokenError::Init(e.clone())),
    }
}

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("tokenizer init: {0}")]
    Init(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_exceeded_at_90_pct() {
        let mut b = Budget::new(1000);
        b.add(Usage {
            input_tokens: 800,
            output_tokens: 99,
            ..Usage::default()
        });
        assert!(!b.exceeded());
        b.add(Usage {
            output_tokens: 1,
            ..Usage::default()
        });
        assert!(b.exceeded());
    }

    #[test]
    fn budget_zero_cap_never_exceeds() {
        let mut b = Budget::new(0);
        b.add(Usage {
            input_tokens: u64::MAX / 2,
            output_tokens: u64::MAX / 2,
            ..Usage::default()
        });
        assert!(!b.exceeded());
    }

    #[test]
    fn null_estimator_is_len_over_4() {
        assert_eq!(NullEstimator.count(""), 0);
        assert_eq!(NullEstimator.count("hello world"), 11 / 4);
    }

    #[test]
    fn tiktoken_counts_nonzero_for_english() {
        let n = TiktokenEstimator.count("hello world");
        assert!(n >= 1, "expected at least 1 token, got {n}");
    }

    #[test]
    fn tiktoken_counts_cjk() {
        let n = TiktokenEstimator.count("안녕하세요");
        assert!(n >= 1, "expected at least 1 token for CJK, got {n}");
    }
}
