//! [`ClaudeCliModel`]: the plan model reached through `claude -p`.
//!
//! The call shape is Ergon's (`src/llm/claude_cli.rs` in the Ergon repo), copied rather than
//! shared: the flags that keep the call lean, the cleared environment that stops a nested
//! session from inheriting this one, the envelope that arrives either as one result object
//! or as an array of events. tt-devpro owns its copy, so a change on Ergon's side never
//! reaches the portal unreviewed.
//!
//! Retries are written out by hand. `api`'s test forbids any dependency whose name carries
//! `retry` or `backoff`, and the rule here is narrow enough not to need one: only a rate
//! limit, a timeout or an empty answer is retried, never an auth failure or a malformed one.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{ModelCall, PlanModel};

/// Tools the planning call must never reach. It reasons over the prompt and nothing else.
const DENIED_TOOLS: &str = "Bash,Read,Write,Edit,Glob,Grep,WebFetch,WebSearch,Task,\
     TodoWrite,TaskCreate,TaskGet,TaskUpdate,TaskList,NotebookEdit,BashOutput,KillShell,\
     Skill,ExitPlanMode";

/// Set by a running Claude Code session. Left in place, the child believes it is nested
/// inside that session and behaves accordingly.
const CLEARED_ENV: [&str; 2] = ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"];

/// One planning call covers the whole window, so it gets the generous bound Ergon uses.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Three attempts in all for a retryable failure, two and four seconds apart.
const MAX_ATTEMPTS: u32 = 3;
const FIRST_BACKOFF: Duration = Duration::from_secs(2);

/// Why a call failed, which decides whether it is worth repeating and what to tell Yurii.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Auth,
    RateLimit,
    Timeout,
    EmptyResponse,
    Parse,
    Process,
    Unknown,
}

impl FailureKind {
    fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimit | Self::Timeout | Self::EmptyResponse)
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Auth => "run `claude /login` and settle again",
            Self::RateLimit => "the model is rate-limited; settle again in a few minutes",
            Self::Timeout | Self::EmptyResponse => {
                "settle again; `echo test | claude -p` checks the CLI on its own"
            }
            Self::Parse => "check `claude --version`; the answer did not have the expected shape",
            Self::Process => "check that `claude` is installed and on PATH",
            Self::Unknown => "settle again; if it repeats, run `claude -p` by hand",
        }
    }
}

/// A failed model call, with the kind that classified it.
#[derive(Debug, thiserror::Error)]
#[error("The plan model failed: {message}\nHint: {}", kind.hint())]
pub struct ModelFailure {
    pub kind: FailureKind,
    pub message: String,
}

fn failure(kind: FailureKind, message: impl Into<String>) -> ModelFailure {
    ModelFailure {
        kind,
        message: message.into(),
    }
}

/// Reads a failure out of the CLI's stderr. Auth is checked first, so an expired login that
/// also mentions a limit is reported as the login it is.
fn classify(stderr: &str) -> FailureKind {
    let lower = stderr.to_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if any(&[
        "authentication",
        "unauthorized",
        "401",
        "login",
        "token expired",
        "not logged in",
    ]) {
        FailureKind::Auth
    } else if any(&[
        "rate limit",
        "429",
        "too many requests",
        "overloaded",
        "capacity",
    ]) {
        FailureKind::RateLimit
    } else if any(&["timeout", "timed out"]) {
        FailureKind::Timeout
    } else {
        FailureKind::Unknown
    }
}

/// The structured answer inside the CLI's output.
///
/// `--output-format json` prints one result object on some CLI versions and an array of
/// stream events ending in a result on others. Both are accepted; a result with no
/// `structured_output` is a parse failure naming what the result said instead.
fn structured_output(response: &Value) -> Result<Value, ModelFailure> {
    let is_result = |event: &Value| event.get("type").and_then(Value::as_str) == Some("result");
    let result = match response {
        Value::Object(_) if is_result(response) => Some(response),
        Value::Array(events) => events.iter().find(|event| is_result(event)),
        _ => None,
    };
    let Some(result) = result else {
        return Err(failure(
            FailureKind::Parse,
            "the CLI printed no result event",
        ));
    };
    match result.get("structured_output") {
        Some(payload) if !payload.is_null() => Ok(payload.clone()),
        _ => {
            let subtype = result
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("none");
            Err(failure(
                FailureKind::Parse,
                format!("the result carried no structured output (subtype: {subtype})"),
            ))
        }
    }
}

/// The plan model behind `claude -p`.
pub struct ClaudeCliModel {
    program: String,
    model: String,
    timeout: Duration,
    first_backoff: Duration,
}

impl ClaudeCliModel {
    /// `model` is the config's `plan_model`, passed to `--model` as written.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            program: "claude".to_string(),
            model: model.into(),
            timeout: CALL_TIMEOUT,
            first_backoff: FIRST_BACKOFF,
        }
    }

    /// The same call against another program and with short bounds, so a test can drive a
    /// real process through every branch without waiting minutes.
    #[cfg(test)]
    fn for_test(program: &str, timeout: Duration) -> Self {
        Self {
            program: program.to_string(),
            model: "haiku".to_string(),
            timeout,
            first_backoff: Duration::from_millis(10),
        }
    }

    async fn call_once(&self, call: &ModelCall) -> Result<Value, ModelFailure> {
        let mut command = Command::new(&self.program);
        command
            .args(["-p", "--model", &self.model])
            .args(["--output-format", "json", "--json-schema", &call.schema])
            .args([
                "--effort",
                "low",
                "--strict-mcp-config",
                "--disable-slash-commands",
            ])
            .args(["--disallowed-tools", DENIED_TOOLS])
            // Away from any project directory, so no CLAUDE.md is loaded into the call.
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in CLEARED_ENV {
            command.env_remove(name);
        }

        let mut child = command.spawn().map_err(|error| {
            failure(
                FailureKind::Process,
                format!("could not start {}: {error}", self.program),
            )
        })?;

        // The prompt is written by its own task while the output is drained. Writing it first
        // and reading after deadlocks once the prompt outgrows the pipe buffer and the child
        // blocks on a full stdout nobody reads yet.
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let prompt = call.prompt.clone();
        let writer = tokio::spawn(async move {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await
        });

        // On timeout the future holding the child is dropped, and `kill_on_drop` ends it.
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                failure(
                    FailureKind::Timeout,
                    format!("no answer in {}s", self.timeout.as_secs()),
                )
            })?
            .map_err(|error| failure(FailureKind::Process, format!("waiting for it: {error}")))?;

        // The exit status is read before the writer's result. A CLI that refuses at once (an
        // expired login) exits without reading a prompt larger than the pipe buffer, the
        // writer then fails with a broken pipe, and reading that first would report "could
        // not write the prompt" in place of the stderr that says why.
        let written = writer.await;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let text = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            return Err(failure(
                classify(&text),
                format!("exit {}: {}", output.status, text.trim()),
            ));
        }

        match written {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(failure(
                    FailureKind::Process,
                    format!("writing the prompt: {error}"),
                ));
            }
            Err(error) => {
                return Err(failure(
                    FailureKind::Process,
                    format!("the prompt writer stopped: {error}"),
                ));
            }
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Err(failure(
                FailureKind::EmptyResponse,
                "the CLI printed nothing",
            ));
        }
        let response: Value = serde_json::from_str(&stdout).map_err(|error| {
            failure(
                FailureKind::Parse,
                format!("the CLI printed something that is not JSON: {error}"),
            )
        })?;
        structured_output(&response)
    }
}

impl PlanModel for ClaudeCliModel {
    async fn complete(&self, call: &ModelCall) -> Result<Value> {
        let mut backoff = self.first_backoff;
        let mut attempt = 1;
        loop {
            match self.call_once(call).await {
                Ok(value) => return Ok(value),
                Err(error) if error.kind.is_retryable() && attempt < MAX_ATTEMPTS => {
                    eprintln!(
                        "\u{23F3} Модель плана ответила ошибкой ({:?}), повтор через {} с",
                        error.kind,
                        backoff.as_secs()
                    );
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                    attempt += 1;
                }
                Err(error) => return Err(anyhow!(error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn call() -> ModelCall {
        ModelCall {
            prompt: "plan these days".to_string(),
            schema: r#"{"type":"object"}"#.to_string(),
        }
    }

    /// A stand-in `claude`: a shell script whose body is given, in a directory of its own.
    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("claude");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        path
    }

    fn kind_of(error: &anyhow::Error) -> FailureKind {
        error
            .downcast_ref::<ModelFailure>()
            .expect("a model failure")
            .kind
    }

    #[test]
    fn a_single_result_object_yields_its_structured_output() {
        let response = json!({"type": "result", "subtype": "success",
            "structured_output": {"days": []}});
        assert_eq!(structured_output(&response).unwrap(), json!({"days": []}));
    }

    #[test]
    fn an_event_array_yields_the_structured_output_of_its_result_event() {
        let response = json!([
            {"type": "system"},
            {"type": "assistant", "structured_output": {"wrong": true}},
            {"type": "result", "structured_output": {"days": [1]}}
        ]);
        assert_eq!(structured_output(&response).unwrap(), json!({"days": [1]}));
    }

    #[test]
    fn a_result_without_structured_output_names_its_subtype() {
        let response = json!({"type": "result", "subtype": "error_max_turns"});
        let error = structured_output(&response).unwrap_err();
        assert_eq!(error.kind, FailureKind::Parse);
        assert!(
            error.message.contains("error_max_turns"),
            "{}",
            error.message
        );
    }

    #[test]
    fn output_with_no_result_event_is_a_parse_failure() {
        let error = structured_output(&json!([{"type": "system"}])).unwrap_err();
        assert_eq!(error.kind, FailureKind::Parse);
    }

    #[test]
    fn stderr_is_classified_with_auth_ahead_of_rate_limits() {
        assert_eq!(classify("401 Unauthorized"), FailureKind::Auth);
        assert_eq!(classify("Not logged in; rate limit"), FailureKind::Auth);
        assert_eq!(classify("429 Too Many Requests"), FailureKind::RateLimit);
        assert_eq!(classify("API overloaded"), FailureKind::RateLimit);
        assert_eq!(classify("request timed out"), FailureKind::Timeout);
        assert_eq!(classify("something else"), FailureKind::Unknown);
    }

    #[test]
    fn only_transient_failures_are_retryable() {
        let retryable: Vec<FailureKind> = [
            FailureKind::Auth,
            FailureKind::RateLimit,
            FailureKind::Timeout,
            FailureKind::EmptyResponse,
            FailureKind::Parse,
            FailureKind::Process,
            FailureKind::Unknown,
        ]
        .into_iter()
        .filter(|kind| kind.is_retryable())
        .collect();
        assert_eq!(
            retryable,
            vec![
                FailureKind::RateLimit,
                FailureKind::Timeout,
                FailureKind::EmptyResponse
            ]
        );
    }

    #[tokio::test]
    async fn the_prompt_goes_to_stdin_and_the_flags_and_environment_are_lean() {
        let dir = TempDir::new().unwrap();
        let seen = dir.path().join("seen");
        let program = script(
            dir.path(),
            &format!(
                "cat > {seen}.stdin\necho \"$@\" > {seen}.argv\n\
                 echo \"cc=${{CLAUDECODE:-unset}} ep=${{CLAUDE_CODE_ENTRYPOINT:-unset}}\" \
                 > {seen}.env\n\
                 echo '{{\"type\":\"result\",\"structured_output\":{{\"ok\":1}}}}'",
                seen = seen.display()
            ),
        );
        let model = ClaudeCliModel::for_test(program.to_str().unwrap(), Duration::from_secs(10));

        // SAFETY: the variables are read only by the child this test spawns.
        unsafe {
            std::env::set_var("CLAUDECODE", "1");
            std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");
        }
        let value = model.complete(&call()).await.unwrap();

        assert_eq!(value, json!({"ok": 1}));
        let read = |ext: &str| std::fs::read_to_string(format!("{}.{ext}", seen.display()));
        assert_eq!(read("stdin").unwrap(), "plan these days");
        let argv = read("argv").unwrap();
        for flag in [
            "-p",
            "--model haiku",
            "--output-format json",
            "--json-schema {\"type\":\"object\"}",
            "--effort low",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--disallowed-tools Bash,Read,",
        ] {
            assert!(argv.contains(flag), "{flag} missing from {argv}");
        }
        assert_eq!(read("env").unwrap().trim(), "cc=unset ep=unset");
    }

    #[tokio::test]
    async fn a_prompt_larger_than_the_pipe_buffer_does_not_deadlock() {
        let dir = TempDir::new().unwrap();
        // Echo the whole prompt back on stdout before reading the rest: the child fills
        // stdout while stdin is still being written.
        let program = script(
            dir.path(),
            "cat >/dev/stderr\necho '{\"type\":\"result\",\"structured_output\":{\"ok\":2}}'",
        );
        let model = ClaudeCliModel::for_test(program.to_str().unwrap(), Duration::from_secs(20));
        let big = ModelCall {
            prompt: "x".repeat(4 * 1024 * 1024),
            schema: "{}".to_string(),
        };
        assert_eq!(model.complete(&big).await.unwrap(), json!({"ok": 2}));
    }

    #[tokio::test]
    async fn a_rate_limit_is_retried_until_the_attempts_run_out() {
        let dir = TempDir::new().unwrap();
        let count = dir.path().join("count");
        let program = script(
            dir.path(),
            &format!(
                "cat >/dev/null\necho x >> {}\necho '429 rate limit' >&2\nexit 1",
                count.display()
            ),
        );
        let model = ClaudeCliModel::for_test(program.to_str().unwrap(), Duration::from_secs(10));

        let error = model.complete(&call()).await.unwrap_err();

        assert_eq!(kind_of(&error), FailureKind::RateLimit);
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 3);
    }

    #[tokio::test]
    async fn an_auth_failure_is_not_retried() {
        let dir = TempDir::new().unwrap();
        let count = dir.path().join("count");
        let program = script(
            dir.path(),
            &format!(
                "cat >/dev/null\necho x >> {}\necho 'Not logged in' >&2\nexit 1",
                count.display()
            ),
        );
        let model = ClaudeCliModel::for_test(program.to_str().unwrap(), Duration::from_secs(10));

        let error = model.complete(&call()).await.unwrap_err();

        assert_eq!(kind_of(&error), FailureKind::Auth);
        assert!(error.to_string().contains("claude /login"), "{error}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);
    }

    /// The CLI refuses before reading its stdin, so writing a prompt larger than the pipe
    /// buffer fails with a broken pipe. The failure is still the login it is.
    #[tokio::test]
    async fn an_auth_failure_before_the_prompt_is_read_is_still_auth() {
        let dir = TempDir::new().unwrap();
        let program = script(dir.path(), "echo 'Not logged in' >&2\nexit 1");
        let model = ClaudeCliModel::for_test(program.to_str().unwrap(), Duration::from_secs(10));
        let big = ModelCall {
            prompt: "x".repeat(4 * 1024 * 1024),
            schema: "{}".to_string(),
        };

        let error = model.complete(&big).await.unwrap_err();

        assert_eq!(kind_of(&error), FailureKind::Auth);
    }

    #[tokio::test]
    async fn a_hung_call_times_out() {
        let dir = TempDir::new().unwrap();
        let program = script(dir.path(), "cat >/dev/null\nsleep 30");
        let model = ClaudeCliModel::for_test(program.to_str().unwrap(), Duration::from_millis(300));

        let error = model.complete(&call()).await.unwrap_err();

        assert_eq!(kind_of(&error), FailureKind::Timeout);
    }

    #[tokio::test]
    async fn a_missing_program_is_a_process_failure() {
        let model = ClaudeCliModel::for_test("/nonexistent/claude", Duration::from_secs(1));
        let error = model.complete(&call()).await.unwrap_err();
        assert_eq!(kind_of(&error), FailureKind::Process);
    }
}
