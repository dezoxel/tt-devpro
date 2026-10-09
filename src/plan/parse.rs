//! Reads Yurii's edits back out of `plan.md`, and compares them with the plan he was shown.
//!
//! Rows are keyed by address, columns by header name, days by the heading the table sits
//! under. The Chrono column is display only and never read: the line behind an address is
//! taken from the stored plan, so a shortened or mangled Chrono cell cannot change it.
//!
//! Anything unreadable is refused with its line number and nothing is guessed. A misread row
//! here is a wrong worklog in DevPro two steps later.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, anyhow, bail};
use chrono::NaiveDate;

use super::render::{
    BILLABLE_MARK, COL_ADDR, COL_HOURS, COL_PROJECT, COL_TITLE, DETAIL_MARK, MEETING_MARK,
    day_label,
};
use super::{LineKind, Plan, PlanLine, Quarters, hours_to_quarters};

/// One data row of a table, as written in the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// 1-based, for error messages.
    pub line_no: usize,
    pub date: NaiveDate,
    /// `None` for a row Yurii added.
    pub addr: Option<u32>,
    pub title: String,
    pub needs_detail: bool,
    pub devpro_project: String,
    pub billable: bool,
    pub quarters: Quarters,
}

/// Column positions, read from a table's header row.
struct Columns {
    addr: usize,
    title: usize,
    project: usize,
    hours: usize,
}

pub fn parse(text: &str, baseline: &Plan) -> Result<Vec<Row>> {
    let mut days: Vec<NaiveDate> = baseline.days.iter().map(|day| day.date).collect();
    days.extend(baseline.errors.iter().map(|error| error.date));
    days.extend(baseline.closed.iter().copied());

    let mut rows = Vec::new();
    let mut day: Option<NaiveDate> = None;
    let mut columns: Option<Columns> = None;

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim();
        let at = |message: String| anyhow!("plan.md line {line_no}: {message}");

        if let Some(label) = heading_label(line) {
            let date = days
                .iter()
                .copied()
                .find(|date| day_label(*date) == label)
                .ok_or_else(|| at(format!("the day '{label}' is not in the plan")))?;
            day = Some(date);
            columns = None;
            continue;
        }
        if !line.starts_with('|') {
            continue;
        }

        let cells = split_row(line);
        if cells.iter().any(|cell| cell == COL_ADDR) {
            columns = Some(header_columns(&cells).map_err(at)?);
            continue;
        }
        if cells
            .iter()
            .all(|cell| !cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':')))
        {
            continue;
        }

        let date = day.ok_or_else(|| at("a table row before any day heading".to_string()))?;
        let columns = columns
            .as_ref()
            .ok_or_else(|| at("a table row without a header row above it".to_string()))?;
        let get = |at_index: usize| cells.get(at_index).map(String::as_str).unwrap_or("");

        let addr = parse_addr(get(columns.addr), &baseline.prefix).map_err(at)?;
        let (title, needs_detail) = strip_title(get(columns.title));
        if title.is_empty() {
            return Err(at("the task title is empty".to_string()));
        }
        let (devpro_project, billable) = strip_project(get(columns.project));
        if devpro_project.is_empty() {
            return Err(at("the DevPro project is empty".to_string()));
        }
        let quarters = parse_hours(get(columns.hours)).map_err(at)?;

        rows.push(Row {
            line_no,
            date,
            addr,
            title,
            needs_detail,
            devpro_project,
            billable,
            quarters,
        });
    }
    Ok(rows)
}

/// The day label of a heading line: the text between the first pair of `**`.
fn heading_label(line: &str) -> Option<&str> {
    let start = line.find("**")? + 2;
    if line.starts_with('|') {
        return None;
    }
    let end = line[start..].find("**")? + start;
    Some(line[start..end].trim())
}

fn header_columns(cells: &[String]) -> Result<Columns, String> {
    let find = |name: &str| {
        cells
            .iter()
            .position(|cell| cell == name)
            .ok_or_else(|| format!("the header has no '{name}' column"))
    };
    Ok(Columns {
        addr: find(COL_ADDR)?,
        title: find(COL_TITLE)?,
        project: find(COL_PROJECT)?,
        hours: find(COL_HOURS)?,
    })
}

/// The cells of a row, trimmed, with `\|` read as a literal pipe.
fn split_row(line: &str) -> Vec<String> {
    let inner = line.strip_prefix('|').unwrap_or(line);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                current.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    cells.push(current);
    cells
        .into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn parse_addr(cell: &str, prefix: &str) -> Result<Option<u32>, String> {
    let cell = cell.trim();
    if cell.is_empty() || cell == "—" || cell == "-" {
        return Ok(None);
    }
    let number = cell.strip_prefix(prefix).unwrap_or(cell).trim();
    number
        .parse::<u32>()
        .map(Some)
        .map_err(|_| format!("'{cell}' is not an address"))
}

/// The title without the markers the renderer put in front of it, and whether ❓ was there.
fn strip_title(cell: &str) -> (String, bool) {
    let mut text = cell.replace("**", "");
    let needs_detail = text.contains(DETAIL_MARK);
    for mark in [MEETING_MARK, DETAIL_MARK] {
        text = text.replace(mark, "");
    }
    (one_line(&text), needs_detail)
}

fn strip_project(cell: &str) -> (String, bool) {
    let text = cell.replace("**", "");
    let billable = text.contains(BILLABLE_MARK);
    (one_line(&text.replace(BILLABLE_MARK, "")), billable)
}

/// The DevPro figure of an hours cell: the number after "→" when there is one.
fn parse_hours(cell: &str) -> Result<Quarters, String> {
    let figure = cell.rsplit('→').next().unwrap_or(cell).replace('*', "");
    let figure = figure.trim().replace(',', ".");
    let value: f64 = figure
        .parse()
        .map_err(|_| format!("'{cell}' is not a number of hours"))?;
    match hours_to_quarters(value) {
        Some(quarters) if quarters > 0 => Ok(quarters),
        _ => Err(format!("{figure} h is not a positive multiple of 0.25")),
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What changed in one day between the plan shown and the file read back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DayEdits {
    /// A shown line and the row that now stands at its address, which differs from it.
    pub edited: Vec<(PlanLine, Row)>,
    pub removed: Vec<PlanLine>,
    pub added: Vec<Row>,
}

/// Compares the rows with the plan they were rendered from. Days with no change are absent.
pub fn diff(baseline: &Plan, rows: &[Row]) -> Result<BTreeMap<NaiveDate, DayEdits>> {
    let mut edits: BTreeMap<NaiveDate, DayEdits> = BTreeMap::new();
    let mut seen: HashSet<u32> = HashSet::new();

    for row in rows {
        let Some(addr) = row.addr else {
            if !baseline.days.iter().any(|day| day.date == row.date) {
                bail!(
                    "plan.md line {}: {} has no plan to add a line to",
                    row.line_no,
                    day_label(row.date)
                );
            }
            edits.entry(row.date).or_default().added.push(row.clone());
            continue;
        };
        let (day, line) = baseline.line(addr).ok_or_else(|| {
            anyhow!(
                "plan.md line {}: {}{addr} is not an address of this plan",
                row.line_no,
                baseline.prefix
            )
        })?;
        if !seen.insert(addr) {
            bail!(
                "plan.md line {}: {}{addr} appears twice",
                row.line_no,
                baseline.prefix
            );
        }
        if day.date != row.date {
            bail!(
                "plan.md line {}: {}{addr} belongs to {}, not {}",
                row.line_no,
                baseline.prefix,
                day_label(day.date),
                day_label(row.date)
            );
        }
        if differs(line, row) {
            if line.kind == LineKind::Recorded {
                bail!(recorded_refusal(row.line_no, &baseline.prefix, addr));
            }
            edits
                .entry(row.date)
                .or_default()
                .edited
                .push((line.clone(), row.clone()));
        }
    }

    for day in &baseline.days {
        for line in &day.lines {
            if seen.contains(&line.addr) {
                continue;
            }
            if line.kind == LineKind::Recorded {
                bail!(
                    "{}{} was removed from plan.md, but it is already in DevPro: delete it there \
                     with `tt-devpro api delete-worklog`, then settle again",
                    baseline.prefix,
                    line.addr
                );
            }
            edits
                .entry(day.date)
                .or_default()
                .removed
                .push(line.clone());
        }
    }
    Ok(edits)
}

/// The stored text is compared in the form the table shows it: a cell is one line with single
/// spaces, so a title with a double or trailing space (a worklog typed in the portal) reads
/// back different from what is stored though nobody touched it.
fn differs(line: &PlanLine, row: &Row) -> bool {
    one_line(&line.title) != row.title
        || one_line(&line.devpro_project) != row.devpro_project
        || line.is_billable() != row.billable
        || line.quarters != row.quarters
        || line.needs_detail != row.needs_detail
}

fn recorded_refusal(line_no: usize, prefix: &str, addr: u32) -> String {
    format!(
        "plan.md line {line_no}: {prefix}{addr} is already in DevPro; change it there with \
         `tt-devpro api update-worklog`, then settle again"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::fixtures::{date, plan};
    use crate::plan::render::render;

    fn rows_of(text: &str) -> Vec<Row> {
        parse(text, &plan()).expect("the text parses")
    }

    #[test]
    fn a_rendered_plan_reads_back_unchanged() {
        let plan = plan();
        let rows = rows_of(&render(&plan));
        assert_eq!(rows.len(), 5);
        assert!(diff(&plan, &rows).unwrap().is_empty());
        assert_eq!(rows[0].addr, Some(1));
        assert_eq!(rows[0].quarters, 20);
        assert!(rows[0].billable);
        assert_eq!(rows[1].title, "D3 - Agentic DQ - Sync");
    }

    #[test]
    fn an_hours_edit_is_one_edited_line() {
        let text = render(&plan()).replace("1.25 → **5.0**", "4.5");
        let edits = diff(&plan(), &rows_of(&text)).unwrap();
        let day = &edits[&date(2026, 10, 5)];
        assert_eq!(day.edited.len(), 1);
        assert_eq!(day.edited[0].1.quarters, 18);
        assert!(day.removed.is_empty() && day.added.is_empty());
    }

    #[test]
    fn a_removed_row_and_an_added_row_are_both_seen() {
        let text = render(&plan()).replace(
            "| Б3 | — | AI cost governance framework | AI Practices | 1.0 |",
            "|  | — | Token spend review | 💵 Inveniam SOW #5 | 1.0 |",
        );
        let edits = diff(&plan(), &rows_of(&text)).unwrap();
        let day = &edits[&date(2026, 10, 5)];
        assert_eq!(day.removed.len(), 1);
        assert_eq!(day.removed[0].title, "AI cost governance framework");
        assert_eq!(day.added.len(), 1);
        assert!(day.added[0].billable);
    }

    #[test]
    fn dropping_the_dollar_mark_changes_billability() {
        let text = render(&plan()).replace(
            "| Connect issue ownership mapping | 💵 Inveniam SOW #5 |",
            "| Connect issue ownership mapping | Inveniam SOW #5 |",
        );
        let edits = diff(&plan(), &rows_of(&text)).unwrap();
        assert!(!edits[&date(2026, 10, 5)].edited[0].1.billable);
    }

    #[test]
    fn editing_a_recorded_line_is_refused() {
        let text = render(&plan()).replace("| Interview |", "| Interviewing |");
        let error = diff(&plan(), &rows_of(&text)).unwrap_err().to_string();
        assert!(error.contains("already in DevPro"), "{error}");
        assert!(error.contains("api update-worklog"), "{error}");
    }

    /// A worklog typed in the portal with a double or trailing space renders as one line
    /// with single spaces. Read back untouched, it must not count as an edit: an edit of a
    /// recorded line is refused, which would block every replan of the plan.
    #[test]
    fn a_recorded_title_with_stray_spaces_reads_back_unchanged() {
        let mut plan = plan();
        let recorded = plan.days[0]
            .lines
            .iter_mut()
            .find(|line| line.kind == LineKind::Recorded)
            .unwrap();
        recorded.title = "Interview  prep ".to_string();
        let rows = parse(&render(&plan), &plan).unwrap();
        assert!(diff(&plan, &rows).unwrap().is_empty());
    }

    #[test]
    fn removing_a_recorded_line_is_refused() {
        let text: String = render(&plan())
            .lines()
            .filter(|l| !l.starts_with("| Б5 "))
            .collect::<Vec<_>>()
            .join("\n");
        let error = diff(&plan(), &rows_of(&text)).unwrap_err().to_string();
        assert!(error.contains("api delete-worklog"), "{error}");
    }

    #[test]
    fn an_unknown_address_is_refused_with_its_line() {
        let text = render(&plan()).replace("| Б2 |", "| Б9 |");
        let error = diff(&plan(), &rows_of(&text)).unwrap_err().to_string();
        assert!(error.contains("line 6") && error.contains("Б9"), "{error}");
    }

    #[test]
    fn hours_off_the_quarter_grid_are_refused() {
        let text = render(&plan()).replace("| 0.5 |\n| Б5", "| 0.4 |\n| Б5");
        let error = parse(&text, &plan()).unwrap_err().to_string();
        assert!(error.contains("multiple of 0.25"), "{error}");
    }

    #[test]
    fn a_row_with_the_address_written_without_prefix_still_matches() {
        let text = render(&plan()).replace("| Б2 |", "| 2 |");
        assert_eq!(rows_of(&text)[1].addr, Some(2));
    }

    #[test]
    fn a_heading_for_a_day_outside_the_plan_is_refused() {
        let text = format!("{}\n\n**Ср 7 октября** — x", render(&plan()));
        let error = parse(&text, &plan()).unwrap_err().to_string();
        assert!(error.contains("Ср 7 октября"), "{error}");
    }

    /// Yurii saying "the title is fine as it is" is the agent deleting the mark; that has to
    /// reach the replan as an edit, or the line stays blocked for `--apply` forever.
    #[test]
    fn removing_the_question_mark_alone_is_an_edit() {
        let mut plan = plan();
        plan.days[0].lines[2].needs_detail = true;
        let text = render(&plan).replace("❓ ", "");
        let edits = diff(&plan, &parse(&text, &plan).unwrap()).unwrap();
        let edited = &edits[&date(2026, 10, 5)].edited;
        assert_eq!(edited.len(), 1);
        assert!(!edited[0].1.needs_detail);
    }
}
