//! `api` — direct portal calls, ported from `ApiCommand.kt`.
//!
//! Everything the five subcommands print is extracted into free functions here, for
//! the reason `renderAssignedProjects` already was one in the incumbent: three of
//! the five mutate the live portal, so no parity run may execute them and a unit
//! test on the renderer is the only oracle their output will ever have.
//!
//! **C32 — three different rules for printing an hour figure, in one file.**
//! `get-worklogs` interpolates a `Double`, so it goes through [`java_dbl`] and a
//! whole `1.0` keeps its `.0` where Rust's `{}` would print `1`. `create-worklog`
//! and `update-worklog` interpolate the *raw option string*, so `--hours 8.00`
//! prints `8.00` and must not be normalised. (The third rule, `%.1f`, lives in
//! `settle.rs`.) The capture at `~/.cache/tt-devpro-rewrite/baseline/api-get-worklogs-0918.out`
//! has `1.0`, `3.0` and `5.0` in it, so this is not a theoretical trap.
//!
//! **A null `expenseType` prints the four characters `null`.** Kotlin interpolates
//! `String?` through `String.valueOf`, which is where `null` comes from; `Option<String>`
//! in Rust does nothing of the kind on its own.

use anyhow::Result;
use chrono::NaiveDate;
use clap::Args;

use crate::fmt::java_dbl;
use crate::model::{CreateWorklogRequest, NormalViewResponse, Project, UpdateWorklogRequest};

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

/// `ApiCommand.kt:38-46`. C8: the date is part of the answer, not context around it.
///
/// The endpoint is `assignedProjectsOnDate`, so a bare list invites being read as
/// "the projects" — which has already produced a false "the client assignments were
/// revoked" diagnosis. An empty result says `  (none)` rather than leaving a header
/// that looks like truncated output.
pub fn render_assigned_projects(date: NaiveDate, projects: &[Project]) -> String {
    let header = format!("Assigned projects as of {date} ({}):", projects.len());
    let mut lines = vec![header];
    if projects.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for project in projects {
            lines.push(format!("  {}: {}", project.short_name, project.unique_id));
        }
    }
    lines.join("\n")
}

/// `ApiCommand.kt:82-96`, every line of it.
///
/// Each day is preceded by a blank line, because the Kotlin is `echo("\n=== … ===")`
/// and `echo` adds the trailing newline itself. The date is printed exactly as the
/// portal returned it — `2026-09-17T00:00:00`, not a reparsed `2026-09-17`.
///
/// Returns the whole body with no trailing newline; the caller prints it with one.
/// A response with no worklogs at all renders the empty string, which is what the
/// incumbent's zero `echo` calls amount to.
pub fn render_worklogs(response: &NormalViewResponse) -> String {
    let mut lines: Vec<String> = Vec::new();
    for page in &response.page_list {
        for day in &page.details_by_dates {
            lines.push(String::new());
            lines.push(format!("=== {} ===", day.date));
            for worklog in &day.worklogs_details {
                lines.push(format!("  Project: {}", worklog.project_short_name));
                lines.push(format!("  Task: {}", worklog.task_title));
                lines.push(format!("  Hours: {}", java_dbl(worklog.logged_hours)));
                lines.push(format!("  Billability: {}", worklog.billability));
                lines.push(format!("  ExpenseType: {}", or_null(&worklog.expense_type)));
                lines.push(format!("  UniqueId: {}", worklog.unique_id));
                lines.push(format!("  ProjectId: {}", worklog.project_unique_id));
                lines.push("  ---".to_string());
            }
        }
    }
    lines.join("\n")
}

/// Kotlin's string interpolation of a nullable reference, which is `String.valueOf`
/// and prints the literal `null`.
fn or_null(value: &Option<String>) -> &str {
    match value {
        Some(text) => text.as_str(),
        None => "null",
    }
}

/// `ApiCommand.kt:128-134`. Printed **before** the call, so it is on stdout even when
/// the write then fails. `hours` is the raw option text, not a parsed number.
pub fn render_create_worklog_header(
    date: &str,
    project_id: &str,
    task: &str,
    hours: &str,
    billability: &str,
    expense_type: &Option<String>,
) -> String {
    [
        "Creating worklog:".to_string(),
        format!("  Date: {date}"),
        format!("  Project: {project_id}"),
        format!("  Task: {task}"),
        format!("  Hours: {hours}"),
        format!("  Billability: {billability}"),
        format!("  ExpenseType: {}", or_null(expense_type)),
    ]
    .join("\n")
}

/// `ApiCommand.kt:176-182`. Same six fields, but the header carries the id and there
/// is no `Id:` line among them — the id appears only in the header.
pub fn render_update_worklog_header(
    id: &str,
    date: &str,
    project_id: &str,
    task: &str,
    hours: &str,
    billability: &str,
    expense_type: &Option<String>,
) -> String {
    [
        format!("Updating worklog: {id}"),
        format!("  Date: {date}"),
        format!("  Project: {project_id}"),
        format!("  Task: {task}"),
        format!("  Hours: {hours}"),
        format!("  Billability: {billability}"),
        format!("  ExpenseType: {}", or_null(expense_type)),
    ]
    .join("\n")
}

/// `ApiCommand.kt:206`.
pub fn render_delete_worklog_header(id: &str) -> String {
    format!("Deleting worklog: {id}")
}

// ---------------------------------------------------------------------------
// Arguments to request bodies
// ---------------------------------------------------------------------------

/// `ApiCommand.kt:118-126`. The mapping the plan calls out as otherwise untested:
/// `WorklogRequestBodyTest` covers request-to-JSON, and nothing covered
/// arguments-to-request.
///
/// `overtime`, `pif` and `googleCalendarEventId` are absent from the CLI and stay
/// `None`, which is what the Kotlin constructor's defaults do.
pub fn create_request(args: &CreateWorklogArgs) -> Result<CreateWorklogRequest> {
    Ok(CreateWorklogRequest {
        worklog_date: args.date.clone(),
        project_unique_id: args.project_id.clone(),
        task_title: args.task.clone(),
        billability: args.billability.clone(),
        duration: parse_hours(&args.hours)?,
        description: args.description.clone(),
        overtime: None,
        expense_type: args.expense_type.clone(),
        pif: None,
        google_calendar_event_id: None,
    })
}

/// `ApiCommand.kt:165-174`. Leads with the id and carries no `googleCalendarEventId`
/// field at all — see C18 on why that asymmetry is the contract rather than an
/// oversight to tidy up.
pub fn update_request(args: &UpdateWorklogArgs) -> Result<UpdateWorklogRequest> {
    Ok(UpdateWorklogRequest {
        unique_id: args.id.clone(),
        worklog_date: args.date.clone(),
        project_unique_id: args.project_id.clone(),
        task_title: args.task.clone(),
        billability: args.billability.clone(),
        duration: parse_hours(&args.hours)?,
        description: args.description.clone(),
        overtime: None,
        expense_type: args.expense_type.clone(),
        pif: None,
    })
}

/// Kotlin's `String.toDouble()`, which is `java.lang.Double.parseDouble`.
///
/// It sits *outside* the incumbent's `try`, so a bad value there throws a raw
/// `NumberFormatException` stack trace and exits 1 — one of C24's two uncaught
/// sites. Under D3 it becomes an ordinary message with a non-zero code.
///
/// The accepted set was measured, not recalled: `~/.cache/tt-devpro-rewrite/measurements/jdk/`
/// holds `ParseProbe.java` and the two answer files. `parseDouble` and Rust's
/// `str::parse::<f64>` disagree in both directions, which is why neither is used
/// alone.
///
/// **Java accepts and Rust does not:** a trailing `d`, `D`, `f` or `F` type suffix
/// (`8d`, `8.5D`), and leading or trailing whitespace, where "whitespace" is every
/// code unit at or below `U+0020` rather than Unicode's definition.
///
/// **Rust accepts and Java does not:** `inf`, `Inf`, `infinity`, `INFINITY`, `nan`,
/// `NAN`. Java takes exactly `Infinity` and `NaN`, and takes them *without* a type
/// suffix — `NaNd` and `Infinityd` are both rejected.
///
/// **Named divergence: hexadecimal floating-point literals.** Java reads `0x1p3` as
/// `8.0`; this port rejects it with the same `For input string:` message. Matching it
/// would mean writing a second, correctly-rounded float parser, and `--hours 0x1p3`
/// is not a form any caller uses. Rejecting is visible; a subtly mis-rounded hex
/// parser would not be.
fn parse_hours(raw: &str) -> Result<f64> {
    java_parse_double(raw).map_err(|message| anyhow::anyhow!(message))
}

/// The measured `Double.parseDouble` surface, including its two error messages.
fn java_parse_double(raw: &str) -> Result<f64, String> {
    // `Double.parseDouble` strips every code unit `<= ' '`, then reports the
    // *stripped* string in its message: `" abc "` fails with `"abc"`.
    let trimmed = raw.trim_matches(|c: char| c <= ' ');
    if trimmed.is_empty() {
        return Err("empty String".to_string());
    }
    parse_trimmed_double(trimmed).ok_or_else(|| format!("For input string: \"{trimmed}\""))
}

fn parse_trimmed_double(trimmed: &str) -> Option<f64> {
    let (negative, body) = match trimmed.strip_prefix(['+', '-']) {
        Some(rest) => (trimmed.starts_with('-'), rest),
        None => (false, trimmed),
    };

    // These two are exact and take no type suffix, so they are settled first.
    if body == "Infinity" {
        return Some(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    if body == "NaN" {
        return Some(f64::NAN);
    }
    if body.starts_with("0x") || body.starts_with("0X") {
        return None;
    }

    let body = body.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(body);
    if body.is_empty() {
        return None;
    }
    // Everything Java spells with letters has been handled above, so any remaining
    // letter other than an exponent marker is Rust being the more permissive of the
    // two — `inf`, `nan` and friends land here and must be refused.
    if body
        .bytes()
        .any(|b| b.is_ascii_alphabetic() && b != b'e' && b != b'E')
    {
        return None;
    }

    let signed = if negative {
        format!("-{body}")
    } else {
        body.to_string()
    };
    signed.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Argument surfaces
// ---------------------------------------------------------------------------

#[derive(Args, Debug, Clone)]
pub struct GetProjectsArgs {
    /// Assignment date (YYYY-MM-DD), defaults to today.
    #[arg(short = 'd', long = "date")]
    pub date: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct GetWorklogsArgs {
    /// Period date (YYYY-MM-DD). Required, and kept as text — the incumbent passes
    /// it to the portal unparsed.
    #[arg(short = 'd', long = "date")]
    pub date: Option<String>,
}

/// C21: `-h` is `--hours` here, and help is `--help` only.
#[derive(Args, Debug, Clone)]
pub struct CreateWorklogArgs {
    #[arg(short = 'd', long = "date")]
    pub date: String,
    #[arg(short = 'p', long = "project-id")]
    pub project_id: String,
    #[arg(short = 't', long = "task")]
    pub task: String,
    #[arg(short = 'b', long = "billability")]
    pub billability: String,
    #[arg(short = 'h', long = "hours")]
    pub hours: String,
    #[arg(short = 'e', long = "expense-type")]
    pub expense_type: Option<String>,
    #[arg(long = "description")]
    pub description: Option<String>,
}

/// C21 again, plus the leading `--id`.
#[derive(Args, Debug, Clone)]
pub struct UpdateWorklogArgs {
    #[arg(short = 'i', long = "id")]
    pub id: String,
    #[arg(short = 'd', long = "date")]
    pub date: String,
    #[arg(short = 'p', long = "project-id")]
    pub project_id: String,
    #[arg(short = 't', long = "task")]
    pub task: String,
    #[arg(short = 'b', long = "billability")]
    pub billability: String,
    #[arg(short = 'h', long = "hours")]
    pub hours: String,
    #[arg(short = 'e', long = "expense-type")]
    pub expense_type: Option<String>,
    #[arg(long = "description")]
    pub description: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DeleteWorklogArgs {
    /// Worklog uniqueId to delete.
    pub id: String,
}
