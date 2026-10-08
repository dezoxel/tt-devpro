//! The plan `settle` shows and `settle --apply` writes: days of worklog lines, hours in quarters.
//!
//! One structure travels the whole loop. `settle` builds it, [`render`] turns it into the
//! markdown table Yurii reads, [`parse`] reads his edits back out of that table, and
//! [`state`] keeps the structure beside the exact text he was shown, so `--apply` can refuse
//! to write anything he has not seen.
//!
//! Hours are whole quarters of an hour ([`Quarters`]), never `f64`. The portal takes hours in
//! steps of 0.25 and a day must come to exactly 8.0; in quarters that is an integer sum to 32,
//! with no rounding step that could make 7.999 pass for 8.

pub mod parse;
pub mod render;
pub mod state;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Hours counted in quarters of an hour: 1.25 h is 5.
pub type Quarters = u32;

/// A full working day, 8.0 h.
pub const DAY_QUARTERS: Quarters = 32;

/// The portal's spelling of a billable line.
pub const BILLABLE: &str = "Billable";

/// The portal's spelling of a non-billable line.
pub const NON_BILLABLE: &str = "NonBillable";

pub fn quarters_to_hours(quarters: Quarters) -> f64 {
    f64::from(quarters) / 4.0
}

/// `hours` in quarters, or `None` when it is negative or not a multiple of 0.25.
///
/// The tolerance only absorbs float noise in a value that is already on the grid (a JSON
/// `1.25` arriving as 1.2500000001); 1.3 is refused rather than rounded, because rounding a
/// number the model or Yurii wrote would put a figure in DevPro that nobody chose.
pub fn hours_to_quarters(hours: f64) -> Option<Quarters> {
    if !hours.is_finite() || hours < 0.0 {
        return None;
    }
    let scaled = hours * 4.0;
    let whole = scaled.round();
    if (scaled - whole).abs() > 1e-6 || whole > f64::from(u32::MAX) {
        return None;
    }
    Some(whole as Quarters)
}

/// Where a line came from, which decides what may change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    /// A Chrono work entry; the model names it and may stretch it.
    Work,
    /// A Chrono meeting; its title and hours come from Chrono, never from the model.
    Meeting,
    /// The main topic of the last days, filling what the day's own entries leave.
    Main,
    /// A filler from the config, the last resort.
    Filler,
    /// A task from the last working days other than the main topic, also a last resort.
    Borrow,
    /// A worklog already in DevPro. Read-only: `--apply` never touches it.
    Recorded,
    /// A line Yurii added by hand in `plan.md`.
    Manual,
}

/// The identity of a Chrono line: entries merge on (project, description) within a day.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChronoKey {
    pub project: String,
    pub description: String,
}

/// One worklog line of a day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanLine {
    /// The number after the prefix, assigned by [`Plan::number`] across the whole plan.
    pub addr: u32,
    pub kind: LineKind,
    /// The Chrono line behind it, for the kinds that have one.
    pub chrono: Option<ChronoKey>,
    /// How many Chrono entries merged into the line.
    pub entry_count: u32,
    /// Hours Chrono logged, unrounded, for the "chrono → DevPro" column.
    pub chrono_hours: Option<f64>,
    pub title: String,
    /// The title is too vague to send as it is; `--apply` refuses until it is rewritten.
    pub needs_detail: bool,
    pub devpro_project: String,
    pub project_id: String,
    pub billability: String,
    pub quarters: Quarters,
    /// The title and hours are fixed: a meeting, a capped override, a recorded worklog, or a
    /// line Yurii edited. A replan keeps them as they are.
    pub pinned: bool,
    /// The DevPro id of a recorded worklog.
    pub worklog_id: Option<String>,
    /// The candidate a main, filler or borrowed line was built from.
    pub candidate_id: Option<String>,
}

impl PlanLine {
    pub fn is_billable(&self) -> bool {
        self.billability == BILLABLE
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayPlan {
    pub date: NaiveDate,
    pub lines: Vec<PlanLine>,
}

impl DayPlan {
    pub fn total_quarters(&self) -> Quarters {
        self.lines.iter().map(|line| line.quarters).sum()
    }
}

/// A day that could not be planned, and why. It stays in the state so a later `--replan`,
/// after the cause is fixed, builds it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayError {
    pub date: NaiveDate,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The section letter in front of every address, `Б` in the morning ritual.
    pub prefix: String,
    /// The last day this plan may write: `--apply` refuses any later one.
    pub cutoff: NaiveDate,
    pub days: Vec<DayPlan>,
    pub errors: Vec<DayError>,
    /// Days of an explicit range that DevPro already holds at 8.0 h.
    pub closed: Vec<NaiveDate>,
}

impl Plan {
    /// Sorts every day's lines the way the table shows them and numbers them through the plan.
    ///
    /// Billable first, then by DevPro project, then by hours descending. Ties keep the order
    /// they came in, so a replan that changes nothing renumbers nothing.
    pub fn number(&mut self) {
        self.days.sort_by_key(|day| day.date);
        self.errors.sort_by_key(|error| error.date);
        self.closed.sort();
        let mut next = 1;
        for day in &mut self.days {
            day.lines.sort_by(|a, b| {
                b.is_billable()
                    .cmp(&a.is_billable())
                    .then_with(|| a.devpro_project.cmp(&b.devpro_project))
                    .then_with(|| b.quarters.cmp(&a.quarters))
            });
            for line in &mut day.lines {
                line.addr = next;
                next += 1;
            }
        }
    }

    pub fn line(&self, addr: u32) -> Option<(&DayPlan, &PlanLine)> {
        self.days.iter().find_map(|day| {
            day.lines
                .iter()
                .find(|line| line.addr == addr)
                .map(|line| (day, line))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.days.is_empty() && self.errors.is_empty() && self.closed.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("a valid date")
    }

    pub fn line(
        kind: LineKind,
        title: &str,
        project: &str,
        billable: bool,
        q: Quarters,
    ) -> PlanLine {
        PlanLine {
            addr: 0,
            kind,
            chrono: None,
            entry_count: 0,
            chrono_hours: None,
            title: title.to_string(),
            needs_detail: false,
            devpro_project: project.to_string(),
            project_id: format!("id-{project}"),
            billability: if billable { BILLABLE } else { NON_BILLABLE }.to_string(),
            quarters: q,
            pinned: matches!(kind, LineKind::Meeting | LineKind::Recorded),
            worklog_id: None,
            candidate_id: None,
        }
    }

    /// `base` with the Chrono line `description` of `chrono_project` behind it.
    pub fn chrono_line(
        base: PlanLine,
        description: &str,
        chrono_project: &str,
        chrono_hours: f64,
    ) -> PlanLine {
        PlanLine {
            chrono: Some(ChronoKey {
                project: chrono_project.to_string(),
                description: description.to_string(),
            }),
            entry_count: 1,
            chrono_hours: Some(chrono_hours),
            ..base
        }
    }

    /// The shape of Monday 2026-10-05: three Inveniam meetings, a short work entry stretched,
    /// the main topic on AI Practices, and a recorded line.
    pub fn monday() -> DayPlan {
        let inveniam = "Inveniam Measurabl - Presales - DevPro - Work";
        DayPlan {
            date: date(2026, 10, 5),
            lines: vec![
                chrono_line(
                    line(
                        LineKind::Meeting,
                        "D3 - Agentic DQ - Sync",
                        "Inveniam SOW #5",
                        true,
                        4,
                    ),
                    "D3 - Agentic DQ - Sync",
                    inveniam,
                    1.0,
                ),
                chrono_line(
                    line(
                        LineKind::Work,
                        "Connect issue ownership mapping",
                        "Inveniam SOW #5",
                        true,
                        20,
                    ),
                    "Map every Connect issue to its producer and its owner",
                    inveniam,
                    1.25,
                ),
                chrono_line(
                    line(LineKind::Meeting, "AI Team Sync", "AI Practices", false, 2),
                    "AI Team Sync",
                    "AI - DevPro - Work",
                    0.5,
                ),
                PlanLine {
                    candidate_id: Some("m1".to_string()),
                    ..line(
                        LineKind::Main,
                        "AI cost governance framework",
                        "AI Practices",
                        false,
                        4,
                    )
                },
                PlanLine {
                    worklog_id: Some("w-1".to_string()),
                    ..line(
                        LineKind::Recorded,
                        "Interview",
                        "Interview Community",
                        false,
                        2,
                    )
                },
            ],
        }
    }

    pub fn plan() -> Plan {
        let mut plan = Plan {
            prefix: "Б".to_string(),
            cutoff: date(2026, 10, 7),
            days: vec![monday()],
            errors: vec![DayError {
                date: date(2026, 10, 6),
                message: "Chrono project 'X - DevPro - Work' has no mapping".to_string(),
            }],
            closed: vec![],
        };
        plan.number();
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn hours_on_the_quarter_grid_convert_exactly() {
        assert_eq!(hours_to_quarters(0.0), Some(0));
        assert_eq!(hours_to_quarters(1.25), Some(5));
        assert_eq!(hours_to_quarters(8.0), Some(32));
        assert_eq!(hours_to_quarters(1.250_000_000_1), Some(5));
        assert_eq!(quarters_to_hours(5), 1.25);
    }

    #[test]
    fn hours_off_the_grid_are_refused_not_rounded() {
        assert_eq!(hours_to_quarters(1.3), None);
        assert_eq!(hours_to_quarters(-0.25), None);
        assert_eq!(hours_to_quarters(f64::NAN), None);
    }

    #[test]
    fn numbering_puts_billable_first_then_project_then_hours_descending() {
        let plan = plan();
        let order: Vec<(u32, &str, Quarters)> = plan.days[0]
            .lines
            .iter()
            .map(|l| (l.addr, l.devpro_project.as_str(), l.quarters))
            .collect();
        assert_eq!(
            order,
            vec![
                (1, "Inveniam SOW #5", 20),
                (2, "Inveniam SOW #5", 4),
                (3, "AI Practices", 4),
                (4, "AI Practices", 2),
                (5, "Interview Community", 2),
            ]
        );
    }

    #[test]
    fn numbering_runs_through_the_days_in_date_order() {
        let mut later = monday();
        later.date = date(2026, 10, 2);
        let mut plan = plan();
        plan.days.push(later);
        plan.number();
        assert_eq!(plan.days[0].date, date(2026, 10, 2));
        assert_eq!(plan.days[1].lines[0].addr, 6);
        assert_eq!(plan.line(6).unwrap().0.date, date(2026, 10, 5));
    }
}
