//! A [`PlanModel`] that answers from a script and remembers what it was asked.

use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::{Result, anyhow};

use super::{ModelCall, PlanModel};

/// Hands out the scripted answers in order and records every call.
///
/// Running out of answers is an error rather than a panic, so a test that expected one call
/// and caused two fails on the count it asserts instead of somewhere inside the planner.
pub struct FakePlanModel {
    answers: Mutex<VecDeque<Result<serde_json::Value>>>,
    calls: Mutex<Vec<ModelCall>>,
}

impl FakePlanModel {
    pub fn new(answers: Vec<Result<serde_json::Value>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<ModelCall> {
        self.calls.lock().expect("calls lock").clone()
    }
}

impl PlanModel for FakePlanModel {
    async fn complete(&self, call: &ModelCall) -> Result<serde_json::Value> {
        self.calls.lock().expect("calls lock").push(call.clone());
        self.answers
            .lock()
            .expect("answers lock")
            .pop_front()
            .unwrap_or_else(|| Err(anyhow!("the fake model has no answer left")))
    }
}
