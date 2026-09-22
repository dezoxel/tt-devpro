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
    let body = body.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(body);
    if body.is_empty() {
        return None;
    }
    // Everything Java spells with letters has been handled above, so any remaining
    // letter other than an exponent marker is Rust being the more permissive of the
    // two — `inf`, `nan` and friends land here and must be refused.
    //
    // It is **not** what refuses the hexadecimal form. An explicit `starts_with("0x")`
    // guard stood here until a mutation round deleted it with every test still green,
    // and a second mutation — relaxing this screen to let `x` and `p` through — also
    // left every test green. Both times the refusal came from the same place it always
    // did: `str::parse::<f64>` does not read hex floats either. The named divergence in
    // `parse_hours`' doc comment is therefore a property of the Rust parser rather than
    // of anything written here, and
    // `tests::a_hexadecimal_float_is_refused_although_the_jvm_reads_it` pins the
    // outcome without claiming a mechanism.
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

/// `--date` is `.convert { LocalDate.parse(it) }.default(LocalDate.now())`
/// (`ApiCommand.kt:52-54`), so the *parse* happens during argument parsing and a bad
/// value is a usage failure rather than a runtime one — see `date-unparseable.err`
/// in the captured baseline. Running the conversion through clap's `value_parser`
/// keeps that ordering instead of re-implementing it in the command body.
#[derive(Args, Debug, Clone)]
pub struct GetProjectsArgs {
    /// Assignment date (YYYY-MM-DD), defaults to today.
    #[arg(short = 'd', long = "date", value_parser = crate::commands::parse_iso_date)]
    pub date: Option<NaiveDate>,
}

/// `--date` here is `.required()` and is **not** converted (`ApiCommand.kt:74`): the
/// text goes to the portal exactly as typed. `api get-worklogs` with no `--date` is
/// the captured `missing-option-worklogs` case, so the field is a plain `String` and
/// clap's own required-ness is what produces it.
#[derive(Args, Debug, Clone)]
pub struct GetWorklogsArgs {
    /// Period date (YYYY-MM-DD).
    #[arg(short = 'd', long = "date")]
    pub date: String,
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

// ---------------------------------------------------------------------------
// Help text
// ---------------------------------------------------------------------------
//
// **D6 — the help text is six constants, not a renderer.**
//
// Clikt lays help out through Mordant, a two-column terminal-formatting engine that
// measures the widest option column, wraps the description column into what is left,
// and adapts both to the real terminal width. A Rust renderer that reproduced it
// byte-for-byte would be a port of Mordant, for output that is fixed at build time
// and that no script parses. So the six texts below are the captured bytes of
// `tt-devpro api … --help`, copied out of `~/.cache/tt-devpro-rewrite/baseline/`.
//
// The one risk a constant carries is drift: rename an option and the help goes on
// advertising the old name, silently. That is what
// [`tests::every_help_constant_lists_exactly_the_options_the_parser_declares`] is
// for — it re-parses each constant's `Options:` block and holds it against the clap
// tree, so the one thing these constants cannot do is disagree with the parser.
//
// Each carries **no trailing newline**, like every other renderer in this file; the
// caller prints it with one. And each one's *first* line is the usage line a parse
// failure prints, which is why there is no second set of usage constants — see
// [`crate::commands::usage_line`].

/// `api --help`, and also what `api` with no subcommand prints — on **stdout**, with
/// exit code **0**. `~/.cache/tt-devpro-rewrite/baseline/cli-errors/api-no-args.out`
/// is byte-identical to `api-help.out`.
pub const API_HELP: &str = r#"Usage: tt-devpro api [<options>] <command> [<args>]...

  Direct API calls to Time Tracking Portal

Options:
  -h, --help  Show this message and exit

Commands:
  get-projects    Get assigned projects as of a date (defaults to today)
  get-worklogs    Get worklogs for a period (normalView endpoint)
  create-worklog  Create a new worklog entry
  update-worklog  Update an existing worklog entry
  delete-worklog  Delete a worklog entry"#;

/// `api get-projects --help`. The metavar is `<value>` here and `<text>` everywhere
/// else: Clikt names it after the converted type, and this is the one option in the
/// group that goes through `.convert { }`.
pub const GET_PROJECTS_HELP: &str = r#"Usage: tt-devpro api get-projects [<options>]

  Get assigned projects as of a date (defaults to today)

Options:
  -d, --date=<value>  Assignment date (YYYY-MM-DD), defaults to today
  -h, --help          Show this message and exit"#;

/// `api get-worklogs --help`.
pub const GET_WORKLOGS_HELP: &str = r#"Usage: tt-devpro api get-worklogs [<options>]

  Get worklogs for a period (normalView endpoint)

Options:
  -d, --date=<text>  Period date (YYYY-MM-DD)
  -h, --help         Show this message and exit"#;

/// `api create-worklog --help`. **C21 is visible in this text**: the help entry is
/// `--help` alone, with no `-h`, because `-h` is `--hours`. Reaching this text with
/// `-h` is impossible — the incumbent answers `Error: option -h requires a value`
/// (`cli-errors/hours-short-flag-is-not-help.err`).
pub const CREATE_WORKLOG_HELP: &str = r#"Usage: tt-devpro api create-worklog [<options>]

  Create a new worklog entry

Options:
  -d, --date=<text>          Worklog date (YYYY-MM-DD)
  -p, --project-id=<text>    Project uniqueId
  -t, --task=<text>          Task title
  -b, --billability=<text>   Billable or NonBillable
  -h, --hours=<text>         Duration in hours
  -e, --expense-type=<text>  Expense type (None, CapEx, OpEx)
  --description=<text>       Optional description
  --help                     Show this message and exit"#;

/// `api update-worklog --help`. C21 again, plus the leading `-i, --id`.
pub const UPDATE_WORKLOG_HELP: &str = r#"Usage: tt-devpro api update-worklog [<options>]

  Update an existing worklog entry

Options:
  -i, --id=<text>            Worklog uniqueId
  -d, --date=<text>          Worklog date (YYYY-MM-DD)
  -p, --project-id=<text>    Project uniqueId
  -t, --task=<text>          Task title
  -b, --billability=<text>   Billable or NonBillable
  -h, --hours=<text>         Duration in hours
  -e, --expense-type=<text>  Expense type (None, CapEx, OpEx)
  --description=<text>       Optional description
  --help                     Show this message and exit"#;

/// `api delete-worklog --help`. The only one of the five with an `Arguments:` block,
/// because the id is positional.
pub const DELETE_WORKLOG_HELP: &str = r#"Usage: tt-devpro api delete-worklog [<options>] <id>

  Delete a worklog entry

Options:
  -h, --help  Show this message and exit

Arguments:
  <id>  Worklog uniqueId to delete"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::usage_line;
    use crate::model::{DateDetails, PageItem, WorklogDetail};
    use clap::Command;

    // -----------------------------------------------------------------------
    // Fixtures, copied out of the captures rather than retyped
    // -----------------------------------------------------------------------

    /// The 2026-09-17 block of `~/.cache/tt-devpro-rewrite/baseline/api-get-worklogs-0918.out`
    /// — its first 42 lines, leading blank line included.
    const CAPTURED_DAY_2026_09_17: &str = r#"
=== 2026-09-17T00:00:00 ===
  Project: Delivery Practices
  Task: AI Practice Daily
  Hours: 0.25
  Billability: Non-billable
  ExpenseType: None
  UniqueId: dc2104e6-d74e-485b-a0eb-b4beabcd9373
  ProjectId: cf84fdca-4809-4678-98b1-2e7cc56537c0
  ---
  Project: Delivery Practices
  Task: Heads Sync
  Hours: 0.5
  Billability: Non-billable
  ExpenseType: None
  UniqueId: 38593b40-cfc9-4883-b4c4-cdf3db95cc30
  ProjectId: cf84fdca-4809-4678-98b1-2e7cc56537c0
  ---
  Project: Inveniam SOW #5
  Task: D3 - Agentic DQ - Sync
  Hours: 1.0
  Billability: Billable
  ExpenseType: None
  UniqueId: 00526fbc-067f-49ba-98e7-a5e436660da8
  ProjectId: cbcbe09f-0190-4fc4-8691-f59bca797f89
  ---
  Project: Inveniam SOW #5
  Task: Map every Connect issue to its producer and its owner
  Hours: 5.25
  Billability: Billable
  ExpenseType: None
  UniqueId: e4f0ed59-7e93-4254-b658-f032b56e5cc8
  ProjectId: cbcbe09f-0190-4fc4-8691-f59bca797f89
  ---
  Project: Inveniam SOW #5
  Task: Weekly Measurabl.ai Check-in
  Hours: 1.0
  Billability: Billable
  ExpenseType: None
  UniqueId: c889dd0f-3ab8-46ea-baaa-6a94640069e5
  ProjectId: cbcbe09f-0190-4fc4-8691-f59bca797f89
  ---"#;

    /// The 2026-09-11 block of the same capture (lines 153-170). Two worklogs, both
    /// with a null `expenseType`, which the 09-17 block does not witness.
    const CAPTURED_DAY_2026_09_11: &str = r#"
=== 2026-09-11T00:00:00 ===
  Project: Delivery Practices
  Task: AI Heads Sync
  Hours: 0.5
  Billability: Non-billable
  ExpenseType: null
  UniqueId: 58e112e6-477b-4d6c-9dba-e9ee62e3a3fd
  ProjectId: cf84fdca-4809-4678-98b1-2e7cc56537c0
  ---
  Project: Inveniam SOW #5
  Task: Map every Connect issue to its producer and its owner
  Hours: 7.5
  Billability: Billable
  ExpenseType: null
  UniqueId: b8bbd5da-c47e-4166-9700-a788a8e6b8c7
  ProjectId: cbcbe09f-0190-4fc4-8691-f59bca797f89
  ---"#;

    /// The first four lines of `~/.cache/tt-devpro-rewrite/baseline/api-get-projects.out`.
    const CAPTURED_PROJECTS_HEAD: &str = r#"Assigned projects as of 2026-09-21 (32):
  AI Practices: ab3b8c28-852e-4325-ac39-f439c5fe17f2
  Delivery Practices: cf84fdca-4809-4678-98b1-2e7cc56537c0
  Dev.Pro Quarterly Webinar: c5e962e5-4bc8-4dbb-9deb-c3eecdd83ed4"#;

    fn project(short_name: &str, unique_id: &str) -> Project {
        Project {
            unique_id: unique_id.to_string(),
            short_name: short_name.to_string(),
            is_internal: false,
            is_favorite: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn worklog(
        project_short_name: &str,
        task_title: &str,
        logged_hours: f64,
        billability: &str,
        expense_type: Option<&str>,
        unique_id: &str,
        project_unique_id: &str,
    ) -> WorklogDetail {
        WorklogDetail {
            unique_id: unique_id.to_string(),
            project_unique_id: project_unique_id.to_string(),
            project_short_name: project_short_name.to_string(),
            task_title: task_title.to_string(),
            billability: billability.to_string(),
            logged_hours,
            is_deletable: true,
            expense_type: expense_type.map(str::to_string),
        }
    }

    fn response(days: Vec<DateDetails>) -> NormalViewResponse {
        NormalViewResponse {
            total_logged_hours: 0.0,
            total_expected_hours: 0.0,
            page_list: vec![PageItem {
                contact_unique_id: "c".to_string(),
                full_name: "Yurii".to_string(),
                logged_hours: 0.0,
                expected_hours: 0.0,
                details_by_dates: days,
            }],
        }
    }

    fn day(date: &str, worklogs: Vec<WorklogDetail>) -> DateDetails {
        DateDetails {
            date: date.to_string(),
            logged_hours: 0.0,
            expected_hours: 8.0,
            worklogs_details: worklogs,
        }
    }

    /// The five worklogs the portal returned for 2026-09-17, transcribed field by
    /// field from the capture.
    fn captured_day_2026_09_17() -> DateDetails {
        const DELIVERY: &str = "cf84fdca-4809-4678-98b1-2e7cc56537c0";
        const INVENIAM: &str = "cbcbe09f-0190-4fc4-8691-f59bca797f89";
        day(
            "2026-09-17T00:00:00",
            vec![
                worklog(
                    "Delivery Practices",
                    "AI Practice Daily",
                    0.25,
                    "Non-billable",
                    Some("None"),
                    "dc2104e6-d74e-485b-a0eb-b4beabcd9373",
                    DELIVERY,
                ),
                worklog(
                    "Delivery Practices",
                    "Heads Sync",
                    0.5,
                    "Non-billable",
                    Some("None"),
                    "38593b40-cfc9-4883-b4c4-cdf3db95cc30",
                    DELIVERY,
                ),
                worklog(
                    "Inveniam SOW #5",
                    "D3 - Agentic DQ - Sync",
                    1.0,
                    "Billable",
                    Some("None"),
                    "00526fbc-067f-49ba-98e7-a5e436660da8",
                    INVENIAM,
                ),
                worklog(
                    "Inveniam SOW #5",
                    "Map every Connect issue to its producer and its owner",
                    5.25,
                    "Billable",
                    Some("None"),
                    "e4f0ed59-7e93-4254-b658-f032b56e5cc8",
                    INVENIAM,
                ),
                worklog(
                    "Inveniam SOW #5",
                    "Weekly Measurabl.ai Check-in",
                    1.0,
                    "Billable",
                    Some("None"),
                    "c889dd0f-3ab8-46ea-baaa-6a94640069e5",
                    INVENIAM,
                ),
            ],
        )
    }

    /// The two worklogs of 2026-09-11, whose `expenseType` the portal sent as null.
    fn captured_day_2026_09_11() -> DateDetails {
        day(
            "2026-09-11T00:00:00",
            vec![
                worklog(
                    "Delivery Practices",
                    "AI Heads Sync",
                    0.5,
                    "Non-billable",
                    None,
                    "58e112e6-477b-4d6c-9dba-e9ee62e3a3fd",
                    "cf84fdca-4809-4678-98b1-2e7cc56537c0",
                ),
                worklog(
                    "Inveniam SOW #5",
                    "Map every Connect issue to its producer and its owner",
                    7.5,
                    "Billable",
                    None,
                    "b8bbd5da-c47e-4166-9700-a788a8e6b8c7",
                    "cbcbe09f-0190-4fc4-8691-f59bca797f89",
                ),
            ],
        )
    }

    // -----------------------------------------------------------------------
    // The help constants against the parser
    // -----------------------------------------------------------------------

    /// One option as the help text advertises it.
    #[derive(Debug, PartialEq)]
    struct HelpOption {
        short: Option<char>,
        long: String,
        takes_value: bool,
    }

    /// Reads the `Options:` block. An entry is `  -d, --date=<text>`, or
    /// `  --description=<text>`, or `  --help`, followed by two or more spaces and the
    /// description. Deliberately tolerant about *where* the description column starts,
    /// because that width is Mordant's business and not a contract; strict about the
    /// names and the `=`, which are.
    fn options_in(help: &str) -> Vec<HelpOption> {
        let mut out = Vec::new();
        let mut inside = false;
        for line in help.split('\n') {
            if line == "Options:" {
                inside = true;
                continue;
            }
            if !inside {
                continue;
            }
            if line.is_empty() || !line.starts_with("  ") {
                break;
            }
            let names = line
                .trim_start()
                .split("  ")
                .next()
                .expect("a names column")
                .trim();
            let mut short = None;
            let mut long = None;
            let mut takes_value = false;
            for name in names.split(", ") {
                let (name, value) = match name.split_once('=') {
                    Some((n, v)) => (n, Some(v)),
                    None => (name, None),
                };
                takes_value |= value.is_some();
                if let Some(rest) = name.strip_prefix("--") {
                    long = Some(rest.to_string());
                } else if let Some(rest) = name.strip_prefix('-') {
                    short = rest.chars().next();
                }
            }
            out.push(HelpOption {
                short,
                long: long.expect("every option line names a long form"),
                takes_value,
            });
        }
        assert!(!out.is_empty(), "no Options: block in:\n{help}");
        out
    }

    /// What clap declares, in declaration order — which is also the order the missing
    /// option lines come out in (`cli-errors/missing-options-create-partial.err`).
    fn options_declared(command: Command) -> Vec<HelpOption> {
        command
            .get_arguments()
            .filter(|arg| arg.get_long().is_some())
            .map(|arg| HelpOption {
                short: arg.get_short(),
                long: arg.get_long().expect("filtered").to_string(),
                takes_value: arg.get_num_args().map(|n| n.takes_values()).unwrap_or(true),
            })
            .collect()
    }

    fn augmented(name: &'static str, augment: fn(Command) -> Command) -> Command {
        augment(Command::new(name).disable_help_flag(true))
    }

    /// A subcommand's name, its help constant, and the function that adds its options
    /// to a `Command` — everything the cross-checks below need about one of the five.
    type HelpCase = (&'static str, &'static str, fn(Command) -> Command);

    fn help_cases() -> [HelpCase; 5] {
        [
            (
                "get-projects",
                GET_PROJECTS_HELP,
                GetProjectsArgs::augment_args,
            ),
            (
                "get-worklogs",
                GET_WORKLOGS_HELP,
                GetWorklogsArgs::augment_args,
            ),
            (
                "create-worklog",
                CREATE_WORKLOG_HELP,
                CreateWorklogArgs::augment_args,
            ),
            (
                "update-worklog",
                UPDATE_WORKLOG_HELP,
                UpdateWorklogArgs::augment_args,
            ),
            (
                "delete-worklog",
                DELETE_WORKLOG_HELP,
                DeleteWorklogArgs::augment_args,
            ),
        ]
    }

    /// The whole reason the help text is allowed to be a constant. Rename
    /// `--project-id` in the `Args` struct and this fails; add an option and this
    /// fails; move which short letter an option claims and this fails. A weaker test
    /// that would let all three through: one that counted the option lines.
    #[test]
    fn every_help_constant_lists_exactly_the_options_the_parser_declares() {
        for (name, help, augment) in help_cases() {
            let advertised: Vec<HelpOption> = options_in(help)
                .into_iter()
                .filter(|option| option.long != "help")
                .collect();
            assert_eq!(
                advertised,
                options_declared(augmented(name, augment)),
                "`api {name}` advertises options its parser does not declare, or the other way round"
            );
        }
    }

    /// C21, stated as the rule that produces it rather than as two special cases: the
    /// help entry keeps `-h` exactly when nothing else has claimed that letter. The
    /// incumbent was measured on this — `api create-worklog -h` answers
    /// `Error: option -h requires a value`, not help.
    #[test]
    fn help_gives_up_its_short_letter_to_whichever_option_claimed_h() {
        let mut taken = 0;
        for (name, help, augment) in help_cases() {
            let h_is_taken = options_declared(augmented(name, augment))
                .iter()
                .any(|option| option.short == Some('h'));
            let help_entry = options_in(help)
                .into_iter()
                .find(|option| option.long == "help")
                .expect("every command offers --help");
            assert_eq!(
                help_entry.short,
                if h_is_taken { None } else { Some('h') },
                "`api {name}`: the -h in its help text does not match who owns -h"
            );
            assert!(!help_entry.takes_value, "`api {name}`: --help is a flag");
            taken += usize::from(h_is_taken);
        }
        assert_eq!(
            taken, 2,
            "exactly create-worklog and update-worklog take -h for --hours"
        );
    }

    /// Measured: `api get-projects -d` fails with a usage block whose first line is
    /// the first line of `api get-projects --help`. See `cli-errors/option-needs-value.err`
    /// and `cli-errors/missing-argument-delete.err`, whose usage line carries `<id>`.
    #[test]
    fn each_help_text_opens_with_the_usage_line_its_failures_print() {
        assert_eq!(
            usage_line(API_HELP),
            "Usage: tt-devpro api [<options>] <command> [<args>]..."
        );
        assert_eq!(
            usage_line(GET_PROJECTS_HELP),
            "Usage: tt-devpro api get-projects [<options>]"
        );
        assert_eq!(
            usage_line(GET_WORKLOGS_HELP),
            "Usage: tt-devpro api get-worklogs [<options>]"
        );
        assert_eq!(
            usage_line(CREATE_WORKLOG_HELP),
            "Usage: tt-devpro api create-worklog [<options>]"
        );
        assert_eq!(
            usage_line(UPDATE_WORKLOG_HELP),
            "Usage: tt-devpro api update-worklog [<options>]"
        );
        assert_eq!(
            usage_line(DELETE_WORKLOG_HELP),
            "Usage: tt-devpro api delete-worklog [<options>] <id>"
        );
    }

    /// In Clikt both come from one `help =` on the command class and cannot disagree.
    /// Here they are six separate constants and can — this is what keeps the group
    /// listing honest when a subcommand's own description is reworded.
    #[test]
    fn the_group_listing_repeats_each_subcommands_own_description() {
        let listed: Vec<(&str, &str)> = API_HELP
            .split('\n')
            .skip_while(|line| *line != "Commands:")
            .skip(1)
            .map(|line| {
                let (name, rest) = line.trim_start().split_once("  ").expect("two columns");
                (name, rest.trim_start())
            })
            .collect();
        let own = |help: &'static str| -> &'static str {
            help.split('\n')
                .nth(2)
                .expect("the description paragraph")
                .trim_start()
        };
        assert_eq!(
            listed,
            vec![
                ("get-projects", own(GET_PROJECTS_HELP)),
                ("get-worklogs", own(GET_WORKLOGS_HELP)),
                ("create-worklog", own(CREATE_WORKLOG_HELP)),
                ("update-worklog", own(UPDATE_WORKLOG_HELP)),
                ("delete-worklog", own(DELETE_WORKLOG_HELP)),
            ]
        );
    }

    /// The order is the order they are registered in `apiSubcommands()`
    /// (`ApiCommand.kt:221-227`), not alphabetical — `get-projects` before
    /// `create-worklog` before `delete-worklog`.
    #[test]
    fn the_group_listing_is_in_registration_order_not_alphabetical() {
        let names: Vec<&str> = API_HELP
            .split('\n')
            .skip_while(|line| *line != "Commands:")
            .skip(1)
            .map(|line| line.trim_start().split("  ").next().expect("a name"))
            .collect();
        assert_eq!(
            names,
            vec![
                "get-projects",
                "get-worklogs",
                "create-worklog",
                "update-worklog",
                "delete-worklog"
            ]
        );
        let mut alphabetical = names.clone();
        alphabetical.sort_unstable();
        assert_ne!(
            names, alphabetical,
            "the two orders must differ, or this test proves nothing"
        );
    }

    /// Which options are required, and in what order — the help text cannot say, because
    /// Clikt marks required options no differently from optional ones (`get-worklogs`'
    /// `-d, --date=<text>` is required and `get-projects`' `-d, --date=<value>` is not,
    /// and the two lines look alike). The oracle is the capture instead:
    /// `cli-errors/missing-options-create.err` lists five `Error: missing option` lines
    /// and `cli-errors/missing-option-worklogs.err` one, each in declaration order —
    /// confirmed to be declaration order rather than command-line order by
    /// `cli-errors/missing-options-create-partial.err`, where `--hours` and `--date` were
    /// supplied in the opposite order and the three that remain still print
    /// `--project-id`, `--task`, `--billability`.
    ///
    /// Without this, making a required option optional passes every other test here.
    #[test]
    fn the_required_options_come_in_the_order_the_captured_failures_list_them() {
        let required = |name: &'static str, augment: fn(Command) -> Command| -> Vec<String> {
            augmented(name, augment)
                .get_arguments()
                .filter(|arg| arg.is_required_set())
                .filter_map(|arg| arg.get_long().map(str::to_string))
                .collect()
        };
        assert_eq!(
            required("get-projects", GetProjectsArgs::augment_args),
            Vec::<String>::new(),
            "`--date` defaults to today"
        );
        assert_eq!(
            required("get-worklogs", GetWorklogsArgs::augment_args),
            vec!["date"]
        );
        assert_eq!(
            required("create-worklog", CreateWorklogArgs::augment_args),
            vec!["date", "project-id", "task", "billability", "hours"]
        );
        assert_eq!(
            required("update-worklog", UpdateWorklogArgs::augment_args),
            vec!["id", "date", "project-id", "task", "billability", "hours"]
        );
        assert_eq!(
            required("delete-worklog", DeleteWorklogArgs::augment_args),
            Vec::<String>::new(),
            "the id is positional, so it is not an option"
        );
    }

    /// `delete-worklog` is the only one of the five with an `Arguments:` block, and its
    /// `<id>` is positional and required — `cli-errors/missing-argument-delete.err` says
    /// `Error: missing argument <id>`, a different sentence from the missing-option one.
    /// [`options_declared`] skips positionals entirely, so nothing else here sees it.
    #[test]
    fn only_delete_worklog_takes_a_positional_and_it_is_the_required_id() {
        for (name, help, augment) in help_cases() {
            let positionals: Vec<(String, bool)> = augmented(name, augment)
                .get_positionals()
                .map(|arg| (arg.get_id().to_string(), arg.is_required_set()))
                .collect();
            let advertised = help.contains("\nArguments:\n");
            if name == "delete-worklog" {
                assert_eq!(positionals, vec![("id".to_string(), true)]);
                assert!(advertised, "delete-worklog advertises its argument");
                assert!(
                    help.ends_with("\n  <id>  Worklog uniqueId to delete"),
                    "got:\n{help}"
                );
            } else {
                assert!(positionals.is_empty(), "`api {name}` grew a positional");
                assert!(!advertised, "`api {name}` advertises an Arguments: block");
            }
        }
    }

    // -----------------------------------------------------------------------
    // render_worklogs
    // -----------------------------------------------------------------------

    /// Pinned against the real thing: every line of the 2026-09-17 block of the
    /// capture, in order, including the blank line that opens it. Implementations that
    /// would pass a looser test and fail this one: any that prints `Hours: 1` for a
    /// whole hour, drops the `  ---` after a day's last worklog, or re-formats the
    /// portal's `2026-09-17T00:00:00` into a date.
    #[test]
    fn the_captured_five_worklog_day_renders_line_for_line() {
        let rendered = render_worklogs(&response(vec![captured_day_2026_09_17()]));
        assert_eq!(rendered, CAPTURED_DAY_2026_09_17);
    }

    /// The separator between two days is exactly one blank line, and the first day
    /// gets one too — `echo("\n=== …")` runs on every day with no special case for the
    /// first. A renderer that joined days with a blank line and skipped the leading one
    /// would produce the same middle and the wrong start.
    #[test]
    fn a_blank_line_opens_every_day_including_the_first() {
        let rendered = render_worklogs(&response(vec![
            captured_day_2026_09_17(),
            captured_day_2026_09_11(),
        ]));
        // Each fixture ends on its last `  ---` and opens with its own newline, so the
        // blank line between two days is the one joined in here. Concatenating them
        // without it asserts a *missing* blank line and fails — which is the point.
        let expected = format!("{CAPTURED_DAY_2026_09_17}\n{CAPTURED_DAY_2026_09_11}");
        assert_eq!(rendered, expected);
        assert!(
            rendered.starts_with("\n=== "),
            "the first day is preceded by a blank line too"
        );
    }

    /// `ExpenseType: null` is in the capture at 2026-09-11, so this is the portal's own
    /// behaviour rather than a hypothetical. Rust's `Option` prints nothing like it.
    #[test]
    fn a_null_expense_type_prints_the_four_characters_null() {
        let rendered = render_worklogs(&response(vec![captured_day_2026_09_11()]));
        assert_eq!(rendered, CAPTURED_DAY_2026_09_11);
        assert_eq!(rendered.matches("  ExpenseType: null").count(), 2);
    }

    /// C32. `{}` on an `f64` gives `1`, `Double.toString` gives `1.0`, and the capture
    /// has `1.0`. The 0.25 row is there so that `{:.1}` cannot pass either.
    #[test]
    fn a_whole_number_of_hours_keeps_its_point_zero() {
        let rendered = render_worklogs(&response(vec![day(
            "2026-09-17T00:00:00",
            vec![
                worklog("P", "t", 1.0, "Billable", Some("None"), "u", "p"),
                worklog("P", "t", 0.25, "Billable", Some("None"), "u", "p"),
            ],
        )]));
        assert!(rendered.contains("\n  Hours: 1.0\n"), "got:\n{rendered}");
        assert!(rendered.contains("\n  Hours: 0.25\n"), "got:\n{rendered}");
    }

    /// The incumbent's loop body simply does not run, so nothing is echoed — not even a
    /// blank line. A day with no worklogs still gets its header, because the header is
    /// outside the inner loop.
    #[test]
    fn nothing_at_all_renders_the_empty_string_but_an_empty_day_keeps_its_header() {
        assert_eq!(render_worklogs(&response(vec![])), "");
        assert_eq!(
            render_worklogs(&response(vec![day("2026-09-17T00:00:00", vec![])])),
            "\n=== 2026-09-17T00:00:00 ==="
        );
    }

    // -----------------------------------------------------------------------
    // render_assigned_projects
    // -----------------------------------------------------------------------

    /// C8. The count in the header is the list's length and the date is the queried
    /// date, both pinned against `api-get-projects.out`, whose header reads
    /// `Assigned projects as of 2026-09-21 (32):`.
    #[test]
    fn the_captured_project_list_renders_its_header_and_rows() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 21).expect("a real date");
        let projects = vec![
            project("AI Practices", "ab3b8c28-852e-4325-ac39-f439c5fe17f2"),
            project("Delivery Practices", "cf84fdca-4809-4678-98b1-2e7cc56537c0"),
            project(
                "Dev.Pro Quarterly Webinar",
                "c5e962e5-4bc8-4dbb-9deb-c3eecdd83ed4",
            ),
        ];
        assert_eq!(
            render_assigned_projects(date, &projects),
            CAPTURED_PROJECTS_HEAD.replace("(32)", "(3)"),
            "only the count differs from the capture's first four lines"
        );
    }

    /// The order is the portal's, not sorted: the capture opens with seven projects in
    /// alphabetical order and then restarts at `AI Practices: SDLC Implementation`, so
    /// sorting the whole list would reorder it.
    #[test]
    fn projects_are_printed_in_the_order_the_portal_sent_them() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 21).expect("a real date");
        let projects = vec![project("Zulu", "z"), project("Alpha", "a")];
        assert_eq!(
            render_assigned_projects(date, &projects),
            "Assigned projects as of 2026-09-21 (2):\n  Zulu: z\n  Alpha: a"
        );
    }

    /// C8's second half: an empty answer must not look like truncated output. The count
    /// is `(0)` and the body is one `  (none)` line.
    #[test]
    fn an_empty_project_list_says_none_rather_than_leaving_a_bare_header() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 15).expect("a real date");
        assert_eq!(
            render_assigned_projects(date, &[]),
            "Assigned projects as of 2026-01-15 (0):\n  (none)"
        );
    }

    /// The date prints as `2026-01-05`, which is `LocalDate.toString` — zero-padded.
    /// `NaiveDate`'s `Display` agrees; its `Debug` and most of its `format!` patterns
    /// would not.
    #[test]
    fn the_header_date_is_the_zero_padded_iso_form() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).expect("a real date");
        let rendered = render_assigned_projects(date, &[]);
        assert!(
            rendered.starts_with("Assigned projects as of 2026-01-05 "),
            "got: {rendered}"
        );
    }

    // -----------------------------------------------------------------------
    // The three write-command headers
    // -----------------------------------------------------------------------

    /// C32's second rule. `--hours 8.00` prints `8.00`: the incumbent interpolates the
    /// option *string*, never the parsed `Double`. Running it through [`java_dbl`]
    /// would print `8.0` and would pass every test whose input was already normalised.
    #[test]
    fn the_create_header_prints_the_hours_option_exactly_as_typed() {
        let rendered = render_create_worklog_header(
            "2026-09-18",
            "cbcbe09f-0190-4fc4-8691-f59bca797f89",
            "Map every Connect issue to its producer and its owner",
            "8.00",
            "Billable",
            &Some("None".to_string()),
        );
        assert_eq!(
            rendered,
            "Creating worklog:\n  Date: 2026-09-18\n  Project: cbcbe09f-0190-4fc4-8691-f59bca797f89\n  Task: Map every Connect issue to its producer and its owner\n  Hours: 8.00\n  Billability: Billable\n  ExpenseType: None"
        );
    }

    /// `--expense-type` is the one optional field among the six printed, and when it is
    /// absent it prints `null` — the same `String.valueOf` rule as the worklog listing.
    /// `--description` is optional too and is not printed at all.
    #[test]
    fn an_absent_expense_type_prints_null_in_both_write_headers() {
        let create = render_create_worklog_header("d", "p", "t", "1", "Billable", &None);
        assert!(create.ends_with("\n  ExpenseType: null"), "got:\n{create}");
        assert!(!create.contains("Description"), "got:\n{create}");
        let update = render_update_worklog_header("w", "d", "p", "t", "1", "Billable", &None);
        assert!(update.ends_with("\n  ExpenseType: null"), "got:\n{update}");
    }

    /// The update header carries the id in its first line and nowhere else — there is
    /// no `  Id:` row among the six. Otherwise the two headers are the same six fields.
    #[test]
    fn the_update_header_carries_the_id_only_in_its_first_line() {
        let update = render_update_worklog_header(
            "5382c445-9a05-453d-92e8-fdf8c0a521d9",
            "2026-09-18",
            "p",
            "t",
            "1.5",
            "Billable",
            &Some("None".to_string()),
        );
        let mut lines = update.split('\n');
        assert_eq!(
            lines.next(),
            Some("Updating worklog: 5382c445-9a05-453d-92e8-fdf8c0a521d9")
        );
        assert_eq!(
            lines.collect::<Vec<_>>(),
            vec![
                "  Date: 2026-09-18",
                "  Project: p",
                "  Task: t",
                "  Hours: 1.5",
                "  Billability: Billable",
                "  ExpenseType: None",
            ]
        );
        assert_eq!(
            update
                .matches("5382c445-9a05-453d-92e8-fdf8c0a521d9")
                .count(),
            1
        );
    }

    /// One line, no field block, and the id verbatim.
    #[test]
    fn the_delete_header_is_one_line_naming_the_id() {
        assert_eq!(
            render_delete_worklog_header("5382c445-9a05-453d-92e8-fdf8c0a521d9"),
            "Deleting worklog: 5382c445-9a05-453d-92e8-fdf8c0a521d9"
        );
    }

    // -----------------------------------------------------------------------
    // Arguments to request bodies
    // -----------------------------------------------------------------------

    fn create_args() -> CreateWorklogArgs {
        CreateWorklogArgs {
            date: "2026-09-18".to_string(),
            project_id: "cbcbe09f-0190-4fc4-8691-f59bca797f89".to_string(),
            task: "Map every Connect issue to its producer and its owner".to_string(),
            billability: "Billable".to_string(),
            hours: "5.25".to_string(),
            expense_type: Some("CapEx".to_string()),
            description: Some("a description".to_string()),
        }
    }

    fn update_args() -> UpdateWorklogArgs {
        UpdateWorklogArgs {
            id: "b8bbd5da-c47e-4166-9700-a788a8e6b8c7".to_string(),
            date: "2026-09-18".to_string(),
            project_id: "cbcbe09f-0190-4fc4-8691-f59bca797f89".to_string(),
            task: "Map every Connect issue to its producer and its owner".to_string(),
            billability: "Billable".to_string(),
            hours: "7.5".to_string(),
            expense_type: None,
            description: None,
        }
    }

    /// The mapping the plan names as otherwise uncovered. Every field is checked,
    /// including the three the CLI cannot set — a builder that quietly sent
    /// `pif: Some(String::new())` would serialise differently and no JSON test would
    /// catch it, because those tests build the struct themselves.
    #[test]
    fn create_arguments_become_the_request_body_field_for_field() {
        let request = create_request(&create_args()).expect("5.25 parses");
        assert_eq!(request.worklog_date, "2026-09-18");
        assert_eq!(
            request.project_unique_id,
            "cbcbe09f-0190-4fc4-8691-f59bca797f89"
        );
        assert_eq!(
            request.task_title,
            "Map every Connect issue to its producer and its owner"
        );
        assert_eq!(request.billability, "Billable");
        assert_eq!(request.duration, 5.25);
        assert_eq!(request.description.as_deref(), Some("a description"));
        assert_eq!(request.expense_type.as_deref(), Some("CapEx"));
        assert_eq!(request.overtime, None);
        assert_eq!(request.pif, None);
        assert_eq!(request.google_calendar_event_id, None);
    }

    /// C18. `UpdateWorklogRequest` has no `googleCalendarEventId` field at all, and the
    /// id lands in `uniqueId` rather than in any field Create shares.
    #[test]
    fn update_arguments_become_the_request_body_and_the_id_lands_in_unique_id() {
        let request = update_request(&update_args()).expect("7.5 parses");
        assert_eq!(request.unique_id, "b8bbd5da-c47e-4166-9700-a788a8e6b8c7");
        assert_eq!(request.worklog_date, "2026-09-18");
        assert_eq!(
            request.project_unique_id,
            "cbcbe09f-0190-4fc4-8691-f59bca797f89"
        );
        assert_eq!(
            request.task_title,
            "Map every Connect issue to its producer and its owner"
        );
        assert_eq!(request.billability, "Billable");
        assert_eq!(request.duration, 7.5);
        assert_eq!(request.description, None);
        assert_eq!(request.expense_type, None);
        assert_eq!(request.overtime, None);
        assert_eq!(request.pif, None);
    }

    /// A bad `--hours` stops the command before anything is printed: the incumbent
    /// builds the request at `ApiCommand.kt:118-126` and only then echoes the header at
    /// `128`. So the failure is the whole output, not a header followed by it — which
    /// is why `create_request` owns the parse rather than the renderer.
    #[test]
    fn a_request_refuses_to_be_built_from_hours_the_jvm_would_reject() {
        let mut args = create_args();
        args.hours = "eight".to_string();
        let error = create_request(&args).expect_err("`eight` is not a number");
        assert_eq!(error.to_string(), "For input string: \"eight\"");
    }

    // -----------------------------------------------------------------------
    // parse_hours == Double.parseDouble
    // -----------------------------------------------------------------------

    /// Every accepting row of `~/.cache/tt-devpro-rewrite/measurements/jdk/parse-double.tsv`,
    /// produced by calling `Double.parseDouble` on GraalVM JDK 21.0.11. `str::parse::<f64>`
    /// on its own fails the four type-suffix rows and the two whitespace rows.
    #[test]
    fn the_hours_parser_accepts_every_finite_form_the_jvm_accepts() {
        let rows: &[(&str, f64)] = &[
            ("8", 8.0),
            ("8.0", 8.0),
            ("8.00", 8.0),
            ("8d", 8.0),
            ("8D", 8.0),
            ("8f", 8.0),
            ("8F", 8.0),
            ("8.5d", 8.5),
            (" 8 ", 8.0),
            ("\t8\t", 8.0),
            ("+8", 8.0),
            ("-8", -8.0),
            (".5", 0.5),
            ("8.", 8.0),
            ("1e3", 1000.0),
            ("1E3", 1000.0),
            ("08", 8.0),
        ];
        for (raw, expected) in rows {
            let parsed = parse_hours(raw).unwrap_or_else(|e| panic!("`{raw}` must parse: {e}"));
            assert_eq!(parsed, *expected, "`{raw}`");
        }
    }

    /// Java takes exactly `Infinity` and `NaN`, with an optional sign and **no** type
    /// suffix. Rust's own parser also takes `inf`, `infinity` and `nan` in any case,
    /// which is the divergence this guards.
    #[test]
    fn the_hours_parser_takes_javas_two_spellings_of_infinity_and_nan_and_no_others() {
        assert_eq!(
            parse_hours("Infinity").expect("Java accepts it"),
            f64::INFINITY
        );
        assert_eq!(
            parse_hours("-Infinity").expect("Java accepts it"),
            f64::NEG_INFINITY
        );
        assert!(parse_hours("NaN").expect("Java accepts it").is_nan());
        for rejected in ["nan", "INFINITY", "inf", "infinity", "Infinityd", "NaNd"] {
            let error = parse_hours(rejected).expect_err("the JVM rejects it");
            assert_eq!(
                error.to_string(),
                format!("For input string: \"{rejected}\"")
            );
        }
    }

    /// Every rejecting row of the same file, message included. The message reports the
    /// **trimmed** string, which is why `"  abc  "` fails naming `abc`.
    #[test]
    fn the_hours_parser_rejects_what_the_jvm_rejects_with_the_jvm_message() {
        for raw in ["1e", "d", "f", "abc", "8abc", "8 5", "1_000", "\u{0668}"] {
            let error = parse_hours(raw).expect_err("the JVM rejects it");
            assert_eq!(
                error.to_string(),
                format!("For input string: \"{raw}\""),
                "`{raw}`"
            );
        }
        let error = parse_hours("  abc  ").expect_err("the JVM rejects it");
        assert_eq!(error.to_string(), "For input string: \"abc\"");
    }

    /// `parseDouble("")` has its own message, and so has a value that is nothing but
    /// whitespace once the `<= ' '` strip has run.
    #[test]
    fn an_empty_or_all_whitespace_value_reports_empty_string_rather_than_the_input() {
        for raw in ["", " ", "\t", "\n", "   \t  "] {
            let error = parse_hours(raw).expect_err("the JVM rejects it");
            assert_eq!(error.to_string(), "empty String", "{raw:?}");
        }
    }

    /// The whitespace Java strips is every code unit `<= ' '`, not Unicode's definition:
    /// a no-break space (U+00A0) is *not* stripped, so the value fails and the message
    /// carries the untouched string. `str::trim` would strip it and this would pass for
    /// the wrong reason.
    #[test]
    fn the_strip_is_code_units_below_space_and_not_unicode_whitespace() {
        assert_eq!(
            parse_hours("\u{000b}8\u{001f}").expect("both are below space"),
            8.0
        );
        let error = parse_hours("\u{00a0}8").expect_err("a no-break space is not stripped");
        assert_eq!(error.to_string(), "For input string: \"\u{00a0}8\"");
    }

    /// **The one named divergence.** Java reads `0x1p3` as `8.0`; this port refuses it
    /// with the ordinary message. Matching it means writing a second, correctly-rounded
    /// float parser for a form no caller of `--hours` uses, and a subtly mis-rounded hex
    /// parser would be invisible where a refusal is not.
    ///
    /// This pins the outcome, not a mechanism: two mutation rounds showed that neither
    /// an explicit `0x` guard nor the alphabetic screen is what produces it — Rust's own
    /// `f64` parser does not read hex floats. The test stays because the behaviour is
    /// the contract; it would go on holding if the reason changed.
    #[test]
    fn a_hexadecimal_float_is_refused_although_the_jvm_reads_it() {
        for raw in ["0x1p3", "0X1P3"] {
            let error = parse_hours(raw).expect_err("this port refuses it");
            assert_eq!(error.to_string(), format!("For input string: \"{raw}\""));
        }
    }
}
