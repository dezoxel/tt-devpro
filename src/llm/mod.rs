//! The model that writes the plan: worklog titles and the hours of every line that is not pinned.
//!
//! The planner builds one prompt and one JSON schema per run and hands them here; what comes back
//! is the structured answer, still unvalidated. Keeping the seam this narrow is what lets the
//! planner's rules be tested against `fake::FakePlanModel` without a process, and lets the
//! real call ([`ClaudeCliModel`]) be tested without a planner.
//!
//! There is no fallback model. When the call fails the run fails with the reason: a plan
//! silently built some other way is a plan Yurii would accept believing it came from here.

pub mod claude_cli;
#[cfg(test)]
pub mod fake;

use anyhow::Result;

pub use claude_cli::ClaudeCliModel;

/// One request to the model: the prompt, and the JSON schema its answer must satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCall {
    pub prompt: String,
    pub schema: String,
}

/// Anything that can answer a [`ModelCall`] with a JSON value.
///
/// Used through generics, never as `dyn`: the one production implementation and the one test
/// implementation are both known at compile time.
pub trait PlanModel {
    async fn complete(&self, call: &ModelCall) -> Result<serde_json::Value>;
}
