//! Token counting + budget tracking. PLAN §3.1 (tiktoken-rs + 0.9 safety factor).

#![forbid(unsafe_code)]

use harness_proto::Usage;
use thiserror::Error;

/// Apply a 0.9 safety factor to model-reported budget (accounts for
/// tokenizer approximation drift between our local estimate and the
/// provider's authoritative count). PLAN §6.
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
        let total = self
            .used
            .input_tokens
            .saturating_add(self.used.output_tokens);
        let cap = ((self.max_total_tokens as f64) * BUDGET_SAFETY_FACTOR) as u64;
        total >= cap
    }
}

/// Estimator trait — real impl wraps `tiktoken_rs::cl100k_base` behind `OnceLock`.
pub trait TokenEstimator: Send + Sync {
    fn count(&self, text: &str) -> usize;
}

/// Default no-op estimator so downstream crates compile before the real
/// tiktoken lazy-init wiring lands.
#[derive(Debug, Default)]
pub struct NullEstimator;

impl TokenEstimator for NullEstimator {
    fn count(&self, text: &str) -> usize {
        // Coarse heuristic — real impl uses tiktoken BPE. Placeholder.
        text.len() / 4
    }
}

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("tokenizer init: {0}")]
    Init(String),
}
