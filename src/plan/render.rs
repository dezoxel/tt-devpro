//! The plan as Yurii reads it: a heading line per day and one markdown table under it.
//!
//! The text is the interface. The morning ritual pastes it as it is, Yurii answers by address
//! ("Б2 half an hour"), the agent edits the row in `plan.md`, and [`super::parse`] reads it
//! back. So every cell [`super::parse`] reads is written here in a form it can read again
//! unambiguously, and the one cell it ignores (the Chrono column) is free to be shortened.
//!
//! Headings and day lines are Russian; only the titles that go to DevPro are English.

use chrono::{Datelike, NaiveDate, Weekday};

use super::{DayError, DayPlan, LineKind, Plan, PlanLine, Quarters, quarters_to_hours};
use crate::service::plan_context::clean_task_title;

pub const COL_ADDR: &str = "Адрес";
pub const COL_CHRONO: &str = "Запись в chrono";
pub const COL_TITLE: &str = "Задача в DevPro";
pub const COL_PROJECT: &str = "Проект DevPro";
pub const COL_HOURS: &str = "Часы";

/// In front of a meeting's title.
pub const MEETING_MARK: &str = "📅";
/// In front of a title too vague to send; `--apply` refuses while one is left.
pub const DETAIL_MARK: &str = "❓";
/// In front of a project the client pays for.
pub const BILLABLE_MARK: &str = "💵";

/// The Chrono cell is for recognising the entry, not for reading it in full.
const CHRONO_CELL_MAX: usize = 50;

/// What the whole output says when there is nothing to settle.
pub const ALL_CLOSED: &str = "Все дни закрыты";

pub fn render(plan: &Plan) -> String {
    if plan.days.is_empty() && plan.errors.is_empty() {
        return if plan.closed.is_empty() {
            ALL_CLOSED.to_string()
        } else {
            plan.closed
                .iter()
                .map(|date| closed_line(*date))
                .collect::<Vec<_>>()
                .join("\n\n")
        };
    }

    // Days, errors and closed days interleave in date order, the way the week reads.
    enum Block<'a> {
        Day(&'a DayPlan),
        Error(&'a DayError),
        Closed(NaiveDate),
    }
    let mut blocks: Vec<(NaiveDate, Block)> = Vec::new();
    blocks.extend(plan.days.iter().map(|day| (day.date, Block::Day(day))));
    blocks.extend(
        plan.errors
            .iter()
            .map(|error| (error.date, Block::Error(error))),
    );
    blocks.extend(plan.closed.iter().map(|date| (*date, Block::Closed(*date))));
    blocks.sort_by_key(|(date, _)| *date);

    blocks
        .iter()
        .map(|(_, block)| match block {
            Block::Day(day) => render_day(day, &plan.prefix),
            Block::Error(error) => format!(
                "\u{26A0}\u{FE0F} **{}** — не спланирован: {}",
                day_label(error.date),
                one_line(&error.message)
            ),
            Block::Closed(date) => closed_line(*date),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn closed_line(date: NaiveDate) -> String {
    format!("**{}** — закрыт в DevPro", day_label(date))
}

/// "Пн 5 октября" — the label a day is shown under, and the one [`super::parse`] matches.
pub fn day_label(date: NaiveDate) -> String {
    let weekday = match date.weekday() {
        Weekday::Mon => "Пн",
        Weekday::Tue => "Вт",
        Weekday::Wed => "Ср",
        Weekday::Thu => "Чт",
        Weekday::Fri => "Пт",
        Weekday::Sat => "Сб",
        Weekday::Sun => "Вс",
    };
    const MONTHS: [&str; 12] = [
        "января",
        "февраля",
        "марта",
        "апреля",
        "мая",
        "июня",
        "июля",
        "августа",
        "сентября",
        "октября",
        "ноября",
        "декабря",
    ];
    format!(
        "{weekday} {} {}",
        date.day(),
        MONTHS[date.month0() as usize]
    )
}

fn render_day(day: &DayPlan, prefix: &str) -> String {
    let chrono_hours: f64 = day.lines.iter().filter_map(|line| line.chrono_hours).sum();
    let billable: Quarters = day
        .lines
        .iter()
        .filter(|line| line.is_billable())
        .map(|line| line.quarters)
        .sum();
    let internal = day.total_quarters() - billable;

    let mut out = vec![
        format!(
            "**{}** — работы в chrono {} ч · {BILLABLE_MARK} {} ч ({}% FTE) · Dev.Pro {} ч ({}%)",
            day_label(day.date),
            hours(chrono_hours),
            hours(quarters_to_hours(billable)),
            fte_percent(billable),
            hours(quarters_to_hours(internal)),
            fte_percent(internal),
        ),
        String::new(),
        format!("| {COL_ADDR} | {COL_CHRONO} | {COL_TITLE} | {COL_PROJECT} | {COL_HOURS} |"),
        "|---|---|---|---|---|".to_string(),
    ];
    for line in &day.lines {
        out.push(format!(
            "| {prefix}{} | {} | {} | {} | {} |",
            line.addr,
            cell(&chrono_cell(line)),
            cell(&title_cell(line)),
            cell(&project_cell(line)),
            hours_cell(line),
        ));
    }
    out.join("\n")
}

/// Share of an 8-hour day, whole percent.
fn fte_percent(quarters: Quarters) -> u32 {
    (f64::from(quarters) * 100.0 / 32.0).round() as u32
}

/// 6.0, 2.75, 0.5 — at most two decimals and never fewer than one.
pub fn hours(value: f64) -> String {
    let fixed = format!("{value:.2}");
    let trimmed = fixed.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{trimmed}0")
    } else {
        trimmed.to_string()
    }
}

fn title_cell(line: &PlanLine) -> String {
    let mut out = String::new();
    if line.kind == LineKind::Meeting {
        out.push_str(MEETING_MARK);
        out.push(' ');
    }
    if line.needs_detail {
        out.push_str(DETAIL_MARK);
        out.push(' ');
    }
    out.push_str(&line.title);
    out
}

fn project_cell(line: &PlanLine) -> String {
    if line.is_billable() {
        format!("{BILLABLE_MARK} {}", line.devpro_project)
    } else {
        line.devpro_project.clone()
    }
}

/// "chrono → DevPro" for a work line, bold when stretched more than twice; one number for
/// everything else, since nothing else has two figures to compare.
fn hours_cell(line: &PlanLine) -> String {
    let devpro = hours(quarters_to_hours(line.quarters));
    match (line.kind, line.chrono_hours) {
        (LineKind::Work, Some(chrono)) => {
            let stretched = quarters_to_hours(line.quarters) > 2.0 * chrono;
            if stretched {
                format!("{} → **{devpro}**", hours(chrono))
            } else {
                format!("{} → {devpro}", hours(chrono))
            }
        }
        _ => devpro,
    }
}

/// "name · first project segment ×N", shortened in the middle to fit 50 characters.
fn chrono_cell(line: &PlanLine) -> String {
    let Some(key) = &line.chrono else {
        return if line.kind == LineKind::Recorded {
            "— в DevPro".to_string()
        } else {
            "—".to_string()
        };
    };
    let mut suffix = format!(" · {}", first_segment(&key.project));
    if line.entry_count > 1 {
        suffix.push_str(&format!(" ×{}", line.entry_count));
    }
    let name = clean_task_title(&key.description, &key.project);
    let budget = CHRONO_CELL_MAX.saturating_sub(suffix.chars().count());
    format!("{}{suffix}", shorten_middle(&name, budget))
}

/// "Inveniam Measurabl - Presales - DevPro - Work" → "Inveniam Measurabl"; the slash form
/// "Coates/Presales/DevPro/Work" → "Coates".
pub fn first_segment(chrono_project: &str) -> &str {
    let dash = chrono_project.split(" - ").next().unwrap_or(chrono_project);
    dash.split('/').next().unwrap_or(dash)
}

/// Keeps whole words from both ends and puts "…" between them, so the start and the end of
/// a long entry name both stay recognisable.
fn shorten_middle(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let width = |head: &[&str], tail: &[&str]| {
        let joined = |part: &[&str]| part.join(" ").chars().count();
        // "head … tail": the ellipsis and the spaces around it.
        joined(head) + joined(tail) + 3
    };
    let (mut head, mut tail): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    let (mut front, mut back) = (0, words.len());
    let mut take_front = true;
    while front < back {
        let fits = if take_front {
            let mut next = head.clone();
            next.push(words[front]);
            width(&next, &tail) <= max
        } else {
            let mut next = tail.clone();
            next.insert(0, words[back - 1]);
            width(&head, &next) <= max
        };
        if !fits {
            break;
        }
        if take_front {
            head.push(words[front]);
            front += 1;
        } else {
            tail.insert(0, words[back - 1]);
            back -= 1;
        }
        take_front = !take_front;
    }
    if head.is_empty() {
        // Even the first word is too long: cut it by characters.
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        return format!("{cut}…");
    }
    if tail.is_empty() {
        return format!("{} …", head.join(" "));
    }
    format!("{} … {}", head.join(" "), tail.join(" "))
}

/// A cell's text with the pipe escaped and line breaks flattened, so it cannot split the row.
fn cell(text: &str) -> String {
    one_line(text).replace('|', "\\|")
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::fixtures::{date, plan};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn a_plan_renders_its_day_line_table_and_error() {
        let text = render(&plan());
        let expected = "\
**Пн 5 октября** — работы в chrono 2.75 ч · 💵 6.0 ч (75% FTE) · Dev.Pro 2.0 ч (25%)

| Адрес | Запись в chrono | Задача в DevPro | Проект DevPro | Часы |
|---|---|---|---|---|
| Б1 | Map every Connect … its owner · Inveniam Measurabl | Connect issue ownership mapping | 💵 Inveniam SOW #5 | 1.25 → **5.0** |
| Б2 | D3 - Agentic DQ - Sync · Inveniam Measurabl | 📅 D3 - Agentic DQ - Sync | 💵 Inveniam SOW #5 | 1.0 |
| Б3 | — | AI cost governance framework | AI Practices | 1.0 |
| Б4 | AI Team Sync · AI | 📅 AI Team Sync | AI Practices | 0.5 |
| Б5 | — в DevPro | Interview | Interview Community | 0.5 |

⚠\u{FE0F} **Вт 6 октября** — не спланирован: Chrono project 'X - DevPro - Work' has no mapping";
        assert_eq!(text, expected);
    }

    #[test]
    fn every_table_row_fits_150_display_columns() {
        for row in render(&plan()).lines().filter(|l| l.starts_with('|')) {
            assert!(row.width() <= 150, "{} columns: {row}", row.width());
        }
    }

    #[test]
    fn the_chrono_cell_never_exceeds_50_characters() {
        let mut plan = plan();
        let line = &mut plan.days[0].lines[0];
        line.chrono.as_mut().unwrap().description =
            "A very long description that keeps going well past any reasonable cell width"
                .to_string();
        line.entry_count = 3;
        let cell = chrono_cell(line);
        assert!(cell.chars().count() <= 50, "{cell}");
        assert!(cell.starts_with("A very"), "{cell}");
        assert!(cell.ends_with("width · Inveniam Measurabl ×3"), "{cell}");
    }

    #[test]
    fn a_needs_detail_title_carries_the_question_mark() {
        let mut plan = plan();
        plan.days[0].lines[2].needs_detail = true;
        assert!(render(&plan).contains("| ❓ AI cost governance framework |"));
    }

    #[test]
    fn a_pipe_in_a_title_is_escaped() {
        let mut plan = plan();
        plan.days[0].lines[2].title = "A | B".to_string();
        assert!(render(&plan).contains("| A \\| B |"));
    }

    #[test]
    fn hours_keep_one_decimal_at_least_and_two_at_most() {
        assert_eq!(hours(6.0), "6.0");
        assert_eq!(hours(2.5), "2.5");
        assert_eq!(hours(2.75), "2.75");
        assert_eq!(hours(0.3667), "0.37");
    }

    #[test]
    fn first_segment_reads_both_project_spellings() {
        assert_eq!(
            first_segment("Inveniam Measurabl - Presales - DevPro - Work"),
            "Inveniam Measurabl"
        );
        assert_eq!(first_segment("Coates/Presales/DevPro/Work"), "Coates");
        assert_eq!(first_segment("AI - DevPro - Work"), "AI");
    }

    #[test]
    fn an_empty_plan_says_all_days_are_closed() {
        let mut plan = plan();
        plan.days.clear();
        plan.errors.clear();
        assert_eq!(render(&plan), ALL_CLOSED);
        plan.closed.push(date(2026, 10, 2));
        assert_eq!(render(&plan), "**Пт 2 октября** — закрыт в DevPro");
    }
}
