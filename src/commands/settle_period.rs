//! The billing-period block `settle` prints under the plan: how far the current DevPro
//! period has gone and how much of the planned billable allocation it has used. Pure; the
//! reads are in `settle.rs`.
//!
//! The boundaries are the portal's own, from its PTR Periods view (`contact/ptrPeriods`),
//! never a 1–15 / 16–end rule: May 2026 ran 1–17 there. The allocation is not in the
//! portal at all, so it comes from `allocations` in the config.
//!
//! The pace reads like the weekly limit in the Claude Code statusline: a bar filled by the
//! share of the allocation used, a `┃` where the period's clock stands, and `↗N%`, the share
//! of the allocation the period ends at if the rate holds.
//!
//! The block is printed, not stored: `plan.md` is the edit surface `--apply` hashes, and a
//! line that changes every day has no place in it.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, NaiveDate, Weekday};

use crate::commands::holidays::is_us_federal_holiday;
use crate::commands::settle::{portal_ids, recorded_ids};
use crate::commands::settle_window::last_settleable_day;
use crate::config::Allocation;
use crate::model::PtrPeriod;
use crate::plan::render::{BILLABLE_MARK, MONTHS_GENITIVE, day_label, hours};
use crate::plan::{LineKind, Plan, quarters_to_hours};
use crate::service::plan_context::PortalDay;

/// The read side's spelling of a billable worklog. The write side spells the other one
/// `NonBillable`, the read side `Non-billable`; billable is the same on both.
const READ_BILLABLE: &str = "Billable";

/// Cells in the pace bar, as in the statusline.
const BAR_CELLS: u32 = 10;

/// Fewer working days than this and an extrapolation is noise, so no pace is claimed. The
/// statusline's `len / 33` guard counts minutes; here the unit is a whole day, and one day
/// of an 11-day period already turns a single full day into a 200% pace.
const MIN_DAYS_FOR_PACE: u32 = 2;

/// The statusline caps the pace at 999%.
const MAX_PACE: u32 = 999;

const MONTHS_ENGLISH: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// One portal billing period.
#[derive(Debug, Clone, PartialEq)]
pub struct Period {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub expected_hours: f64,
}

/// The last day the block counts: today when the plan reaches today (`--include-today`, or
/// an explicit range up to today or later), else yesterday. Read from the stored cutoff, so
/// `settle` and `--replan` agree.
pub fn through(plan_cutoff: NaiveDate, today: NaiveDate) -> NaiveDate {
    last_settleable_day(today, plan_cutoff >= today)
}

/// `"October 01 - 15, 2026"` → 1–15 October 2026. A label naming two months
/// (`"October 26 - November 10, 2026"`) is read the same way; none has been seen yet.
pub fn parse_label(label: &str) -> Option<(NaiveDate, NaiveDate)> {
    let (range, year) = label.rsplit_once(',')?;
    let year: i32 = year.trim().parse().ok()?;
    let (from, to) = range.split_once(" - ")?;
    let mut from = from.split_whitespace();
    let start_month = month_number(from.next()?)?;
    let start_day: u32 = from.next()?.parse().ok()?;
    if from.next().is_some() {
        return None;
    }
    let to: Vec<&str> = to.split_whitespace().collect();
    let (end_month, end_day) = match to.as_slice() {
        [day] => (start_month, day.parse().ok()?),
        [month, day] => (month_number(month)?, day.parse().ok()?),
        _ => return None,
    };
    let start = NaiveDate::from_ymd_opt(year, start_month, start_day)?;
    let end = NaiveDate::from_ymd_opt(year, end_month, end_day)?;
    (start <= end).then_some((start, end))
}

fn month_number(name: &str) -> Option<u32> {
    let index = MONTHS_ENGLISH.iter().position(|month| *month == name)?;
    Some(index as u32 + 1)
}

/// The period that holds `date`. Several can: on 2026-05-20 the portal answered both
/// `May 01 - 17` and `May 01 - 31`, and the shorter is the one that is billed. A label this
/// code cannot read stops the choice rather than being skipped, since it might be the one.
pub fn choose(periods: &[PtrPeriod], date: NaiveDate) -> Result<Period> {
    let mut parsed = Vec::new();
    for period in periods {
        let (start, end) = parse_label(&period.ptr_period).ok_or_else(|| {
            anyhow!(
                "the portal's period label {:?} is unreadable",
                period.ptr_period
            )
        })?;
        parsed.push(Period {
            start,
            end,
            expected_hours: period.expected_hours,
        });
    }
    let chosen = parsed
        .into_iter()
        .filter(|period| period.start <= date && date <= period.end)
        .min_by_key(|period| period.end - period.start);
    match chosen {
        Some(period) => Ok(period),
        None => {
            let labels: Vec<&str> = periods.iter().map(|p| p.ptr_period.as_str()).collect();
            bail!("no portal period holds {date}: {labels:?}")
        }
    }
}

/// Weekdays in `[start, end]` that are not holidays.
///
/// The holiday rule is the local one, eight holidays on purpose (`holidays.rs`), while the
/// period's total comes from the portal. In a period with a holiday the portal observes and
/// this rule does not (Columbus Day, Veterans Day, Presidents' Day), the count runs one day
/// high after it. 1–15 Sep 2026 (Labor Day, 80 h) and 1–15 Oct 2026 (Columbus Day worked,
/// 88 h) agree with the portal.
pub fn working_days(start: NaiveDate, end: NaiveDate) -> u32 {
    start
        .iter_days()
        .take_while(|day| *day <= end)
        .filter(|day| !matches!(day.weekday(), Weekday::Sat | Weekday::Sun))
        .filter(|day| !is_us_federal_holiday(*day))
        .count() as u32
}

/// Billable hours of one project so far: in DevPro, and planned but not yet written.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Billable {
    pub recorded: f64,
    pub planned: f64,
}

impl Billable {
    fn total(self) -> f64 {
        self.recorded + self.planned
    }
}

/// Billable hours per DevPro project in `[start, through]`. A plan line already in DevPro
/// (`Recorded`) is counted from DevPro and only from there.
///
/// A plan day whose worklogs in DevPro are no longer the ones it recorded is not counted
/// from the plan: someone wrote to it since, `--apply` would skip it, and its planned lines
/// would count a second time next to what DevPro now holds. A planned line is named as
/// DevPro names its project id, so a project the config spells differently (a renamed one
/// resolved through `project_ids`, another case) stays one line.
pub fn billable_hours(
    portal: &[PortalDay],
    plan: &Plan,
    start: NaiveDate,
    through: NaiveDate,
) -> BTreeMap<String, Billable> {
    let within = |date: NaiveDate| start <= date && date <= through;
    let names: HashMap<&str, &str> = portal
        .iter()
        .flat_map(|day| &day.worklogs)
        .map(|w| (w.project_unique_id.as_str(), w.project_short_name.as_str()))
        .collect();
    let mut by_project: BTreeMap<String, Billable> = BTreeMap::new();
    for day in portal.iter().filter(|day| within(day.date)) {
        for worklog in day
            .worklogs
            .iter()
            .filter(|w| w.billability == READ_BILLABLE)
        {
            let entry = by_project
                .entry(worklog.project_short_name.clone())
                .or_default();
            entry.recorded += worklog.logged_hours;
        }
    }
    for day in plan.days.iter().filter(|day| within(day.date)) {
        // The same test `--apply` makes before writing a day.
        let held = portal.iter().find(|portal_day| portal_day.date == day.date);
        if recorded_ids(day) != portal_ids(held) {
            continue;
        }
        for line in &day.lines {
            if line.kind != LineKind::Recorded && line.is_billable() {
                let name = names
                    .get(line.project_id.as_str())
                    .copied()
                    .unwrap_or(&line.devpro_project);
                let entry = by_project.entry(name.to_string()).or_default();
                entry.planned += quarters_to_hours(line.quarters);
            }
        }
    }
    by_project
}

/// The block, line by line, starting with an empty line that keeps it off the table.
pub fn block(
    period: &Period,
    through: NaiveDate,
    portal: &[PortalDay],
    plan: &Plan,
    allocations: &[Allocation],
) -> Vec<String> {
    let total_days = (period.expected_hours / 8.0).round() as u32;
    let elapsed_days = working_days(period.start, through.min(period.end)).min(total_days);
    let elapsed = if total_days == 0 {
        0.0
    } else {
        f64::from(elapsed_days) / f64::from(total_days)
    };

    let mut out = vec![
        String::new(),
        format!(
            "**Период {}** — прошло {elapsed_days} из {total_days} рабочих дней ({}%), по {}",
            period_name(period.start, period.end),
            percent(elapsed),
            day_label(through)
        ),
    ];

    let mut billable = billable_hours(portal, plan, period.start, through);
    for allocation in allocations {
        billable
            .entry(allocation.devpro_project.clone())
            .or_default();
    }
    if billable.is_empty() {
        out.push(format!("- {BILLABLE_MARK} billable-часов в периоде нет"));
    }
    for (project, so_far) in &billable {
        let allocation = allocations.iter().find(|a| &a.devpro_project == project);
        out.push(project_line(
            project,
            *so_far,
            allocation,
            period,
            elapsed_days,
            elapsed,
        ));
    }

    let unplanned = plan
        .errors
        .iter()
        .filter(|error| period.start <= error.date && error.date <= through)
        .count();
    if unplanned > 0 {
        out.push(format!(
            "- ⚠️ не спланировано {unplanned} дн. в периоде — их плановые часы не учтены"
        ));
    }
    out
}

fn project_line(
    project: &str,
    so_far: Billable,
    allocation: Option<&Allocation>,
    period: &Period,
    elapsed_days: u32,
    elapsed: f64,
) -> String {
    let logged = so_far.total();
    let fte = (elapsed_days > 0).then(|| logged / (8.0 * f64::from(elapsed_days)));
    let mut line = format!("- {BILLABLE_MARK} {project} — ");
    match allocation {
        Some(allocation) => {
            let allocated = allocation.fte * period.expected_hours;
            let used = if allocated > 0.0 {
                logged / allocated
            } else {
                0.0
            };
            line.push_str(&format!(
                "{} из {} ч ({}%) {}",
                hours(logged),
                hours(allocated),
                percent(used),
                bar(used, elapsed_days > 0, elapsed)
            ));
            if elapsed_days >= MIN_DAYS_FOR_PACE {
                let pace = ((used / elapsed * 100.0).round() as u32).min(MAX_PACE);
                line.push_str(&format!(" ↗{pace}%"));
            }
            if let Some(fte) = fte {
                line.push_str(&format!(
                    " · {} FTE из {}",
                    hours(fte),
                    hours(allocation.fte)
                ));
            }
        }
        None => {
            line.push_str(&format!("{} ч", hours(logged)));
            if let Some(fte) = fte {
                line.push_str(&format!(" · {} FTE", hours(fte)));
            }
            line.push_str(
                " · плановой аллокации нет: allocations в ~/.config/tt-devpro/config.yaml",
            );
        }
    }
    if so_far.planned > 0.0 {
        line.push_str(&format!(" · из них в плане {} ч", hours(so_far.planned)));
    }
    line
}

/// The statusline's `win_bar`: `█` for the share used, `░` for the rest, and `┃` between
/// cells where the clock stands, kept off both ends where it would separate nothing.
fn bar(used: f64, started: bool, elapsed: f64) -> String {
    let filled = ((used * f64::from(BAR_CELLS)).floor() as u32).min(BAR_CELLS);
    let tick =
        started.then(|| ((elapsed * f64::from(BAR_CELLS)).round() as u32).clamp(1, BAR_CELLS - 1));
    let mut out = String::new();
    for cell in 0..BAR_CELLS {
        if tick == Some(cell) {
            out.push('┃');
        }
        out.push(if cell < filled { '█' } else { '░' });
    }
    out
}

/// «1–15 октября», or «26 октября – 10 ноября» across a month.
fn period_name(start: NaiveDate, end: NaiveDate) -> String {
    let month = |date: NaiveDate| MONTHS_GENITIVE[date.month0() as usize];
    if start.month() == end.month() {
        format!("{}–{} {}", start.day(), end.day(), month(end))
    } else {
        format!(
            "{} {} – {} {}",
            start.day(),
            month(start),
            end.day(),
            month(end)
        )
    }
}

fn percent(share: f64) -> u32 {
    (share * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorklogDetail;
    use crate::plan::fixtures::date;
    use crate::plan::{BILLABLE, ChronoKey, DayError, DayPlan, NON_BILLABLE, PlanLine};

    fn ptr(label: &str, expected: f64) -> PtrPeriod {
        PtrPeriod {
            ptr_period: label.to_string(),
            expected_hours: expected,
        }
    }

    fn october_first_half() -> Period {
        Period {
            start: date(2026, 10, 1),
            end: date(2026, 10, 15),
            expected_hours: 88.0,
        }
    }

    fn worklog(project: &str, billability: &str, hours: f64) -> WorklogDetail {
        WorklogDetail {
            unique_id: "w".to_string(),
            project_unique_id: project.to_string(),
            project_short_name: project.to_string(),
            task_title: "t".to_string(),
            billability: billability.to_string(),
            logged_hours: hours,
            is_deletable: true,
            expense_type: None,
        }
    }

    fn portal_day(day: NaiveDate, worklogs: Vec<WorklogDetail>) -> PortalDay {
        PortalDay {
            date: day,
            logged_hours: worklogs.iter().map(|w| w.logged_hours).sum(),
            worklogs,
        }
    }

    fn line(kind: LineKind, project: &str, billability: &str, quarters: u32) -> PlanLine {
        PlanLine {
            addr: 0,
            kind,
            chrono: Some(ChronoKey {
                project: "c".to_string(),
                description: "d".to_string(),
            }),
            entry_count: 1,
            chrono_hours: None,
            title: "t".to_string(),
            needs_detail: false,
            devpro_project: project.to_string(),
            project_id: project.to_string(),
            billability: billability.to_string(),
            quarters,
            pinned: false,
            edited: false,
            worklog_id: None,
            candidate_id: None,
        }
    }

    fn recorded_line(worklog_id: &str, project: &str, quarters: u32) -> PlanLine {
        PlanLine {
            worklog_id: Some(worklog_id.to_string()),
            ..line(LineKind::Recorded, project, BILLABLE, quarters)
        }
    }

    fn plan(days: Vec<DayPlan>, errors: Vec<DayError>) -> Plan {
        Plan {
            prefix: "Б".to_string(),
            cutoff: date(2026, 10, 8),
            days,
            errors,
            closed: Vec::new(),
            removed: BTreeMap::new(),
        }
    }

    fn inveniam(fte: f64) -> Vec<Allocation> {
        vec![Allocation {
            devpro_project: "Inveniam SOW #5".to_string(),
            fte,
        }]
    }

    // -- the period ----------------------------------------------------------

    /// The labels the portal returned on 2026-10-09 and 2026-09-20: the current and the
    /// previous period, as PTR Periods shows them.
    #[test]
    fn the_measured_labels_read_as_their_boundaries() {
        let cases = [
            ("October 01 - 15, 2026", (2026, 10, 1), (2026, 10, 15)),
            ("October 16 - 31, 2026", (2026, 10, 16), (2026, 10, 31)),
            ("September 01 - 15, 2026", (2026, 9, 1), (2026, 9, 15)),
            ("September 16 - 30, 2026", (2026, 9, 16), (2026, 9, 30)),
            ("May 01 - 17, 2026", (2026, 5, 1), (2026, 5, 17)),
        ];
        for (label, (y1, m1, d1), (y2, m2, d2)) in cases {
            assert_eq!(
                parse_label(label),
                Some((date(y1, m1, d1), date(y2, m2, d2))),
                "{label}"
            );
        }
    }

    #[test]
    fn a_label_across_two_months_names_both() {
        assert_eq!(
            parse_label("October 26 - November 10, 2026"),
            Some((date(2026, 10, 26), date(2026, 11, 10)))
        );
    }

    #[test]
    fn a_label_in_another_shape_is_unreadable() {
        for label in [
            "2026-10-01 - 2026-10-15",
            "Oct 01 - 15, 2026",
            "October 15 - 01, 2026",
            "",
        ] {
            assert_eq!(parse_label(label), None, "{label}");
        }
    }

    #[test]
    fn the_period_holding_the_date_is_chosen() {
        let periods = [
            ptr("October 01 - 15, 2026", 88.0),
            ptr("October 16 - 31, 2026", 88.0),
        ];
        assert_eq!(
            choose(&periods, date(2026, 10, 8)).unwrap(),
            october_first_half()
        );
        assert_eq!(
            choose(&periods, date(2026, 10, 16)).unwrap().start,
            date(2026, 10, 16)
        );
    }

    /// The portal's answer on 2026-05-20: the month and its first half overlap.
    #[test]
    fn of_two_periods_holding_the_date_the_shorter_is_chosen() {
        let periods = [
            ptr("May 01 - 31, 2026", 160.0),
            ptr("May 01 - 17, 2026", 88.0),
        ];
        let chosen = choose(&periods, date(2026, 5, 12)).unwrap();
        assert_eq!(
            (chosen.end, chosen.expected_hours),
            (date(2026, 5, 17), 88.0)
        );
        let chosen = choose(&periods, date(2026, 5, 20)).unwrap();
        assert_eq!(chosen.end, date(2026, 5, 31));
    }

    #[test]
    fn an_unreadable_label_or_no_period_is_an_error() {
        let message = choose(&[ptr("Q4 2026", 500.0)], date(2026, 10, 8))
            .unwrap_err()
            .to_string();
        assert!(message.contains("\"Q4 2026\" is unreadable"), "{message}");
        let message = choose(&[ptr("September 01 - 15, 2026", 80.0)], date(2026, 10, 8))
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("no portal period holds 2026-10-08"),
            "{message}"
        );
    }

    #[test]
    fn through_is_yesterday_unless_the_plan_reaches_today() {
        let today = date(2026, 10, 9);
        assert_eq!(through(date(2026, 10, 8), today), date(2026, 10, 8));
        assert_eq!(through(date(2026, 9, 30), today), date(2026, 10, 8));
        assert_eq!(through(today, today), today);
        assert_eq!(through(date(2026, 10, 20), today), today);
    }

    /// The portal's expected hours on both measured periods: 88 h and 80 h (Labor Day).
    #[test]
    fn working_days_skip_weekends_and_holidays() {
        assert_eq!(working_days(date(2026, 10, 1), date(2026, 10, 15)), 11);
        assert_eq!(working_days(date(2026, 9, 1), date(2026, 9, 15)), 10);
        assert_eq!(working_days(date(2026, 10, 1), date(2026, 10, 8)), 6);
        assert_eq!(working_days(date(2026, 10, 9), date(2026, 10, 8)), 0);
    }

    // -- the hours -----------------------------------------------------------

    /// A recorded plan line is the DevPro worklog itself: counting both would double it.
    #[test]
    fn a_plan_line_already_in_devpro_is_counted_once() {
        let portal = [portal_day(
            date(2026, 10, 6),
            vec![worklog("Inveniam SOW #5", "Billable", 2.0)],
        )];
        let plan = plan(
            vec![DayPlan {
                date: date(2026, 10, 6),
                lines: vec![
                    recorded_line("w", "Inveniam SOW #5", 8),
                    line(LineKind::Work, "Inveniam SOW #5", BILLABLE, 12),
                ],
            }],
            Vec::new(),
        );
        let hours = billable_hours(&portal, &plan, date(2026, 10, 1), date(2026, 10, 8));
        assert_eq!(
            hours["Inveniam SOW #5"],
            Billable {
                recorded: 2.0,
                planned: 3.0
            }
        );
    }

    /// The day was planned empty, then written in the portal by hand: the plan no longer
    /// describes it, and counting both would double the day.
    #[test]
    fn a_plan_day_written_to_devpro_since_is_counted_from_devpro_only() {
        let portal = [portal_day(
            date(2026, 10, 6),
            vec![worklog("Inveniam SOW #5", "Billable", 8.0)],
        )];
        let plan = plan(
            vec![DayPlan {
                date: date(2026, 10, 6),
                lines: vec![line(LineKind::Work, "Inveniam SOW #5", BILLABLE, 32)],
            }],
            Vec::new(),
        );
        let hours = billable_hours(&portal, &plan, date(2026, 10, 1), date(2026, 10, 8));
        assert_eq!(
            hours["Inveniam SOW #5"],
            Billable {
                recorded: 8.0,
                planned: 0.0
            }
        );
    }

    /// The config may spell a project otherwise than DevPro does (a renamed project resolved
    /// through `project_ids`); the id is the same, so it is one project.
    #[test]
    fn a_planned_line_takes_the_name_devpro_gives_its_project_id() {
        let portal = [portal_day(
            date(2026, 10, 1),
            vec![worklog("Inveniam SOW #5", "Billable", 2.0)],
        )];
        let mut renamed = line(LineKind::Work, "Inveniam SOW 5 (old)", BILLABLE, 4);
        renamed.project_id = "Inveniam SOW #5".to_string();
        let plan = plan(
            vec![DayPlan {
                date: date(2026, 10, 7),
                lines: vec![renamed],
            }],
            Vec::new(),
        );
        let hours = billable_hours(&portal, &plan, date(2026, 10, 1), date(2026, 10, 8));
        assert_eq!(hours.len(), 1);
        assert_eq!(hours["Inveniam SOW #5"].total(), 3.0);
    }

    #[test]
    fn only_billable_hours_inside_the_window_count() {
        let portal = [
            portal_day(
                date(2026, 9, 30),
                vec![worklog("Inveniam SOW #5", "Billable", 8.0)],
            ),
            portal_day(
                date(2026, 10, 1),
                vec![
                    worklog("Inveniam SOW #5", "Billable", 3.0),
                    worklog("AI Practices", "Non-billable", 5.0),
                ],
            ),
            portal_day(
                date(2026, 10, 9),
                vec![worklog("Inveniam SOW #5", "Billable", 8.0)],
            ),
        ];
        let plan = plan(
            vec![DayPlan {
                date: date(2026, 10, 7),
                lines: vec![line(LineKind::Work, "AI Practices", NON_BILLABLE, 32)],
            }],
            Vec::new(),
        );
        let hours = billable_hours(&portal, &plan, date(2026, 10, 1), date(2026, 10, 8));
        assert_eq!(hours.len(), 1);
        assert_eq!(hours["Inveniam SOW #5"].total(), 3.0);
    }

    // -- the block -----------------------------------------------------------

    /// 9 October 2026 morning: 6 of 11 days gone, 18 billable hours of a 0.5 allocation.
    #[test]
    fn the_block_shows_the_period_and_the_pace_against_the_allocation() {
        let portal = [portal_day(
            date(2026, 10, 1),
            vec![worklog("Inveniam SOW #5", "Billable", 12.5)],
        )];
        let plan = plan(
            vec![DayPlan {
                date: date(2026, 10, 7),
                lines: vec![line(LineKind::Main, "Inveniam SOW #5", BILLABLE, 22)],
            }],
            Vec::new(),
        );
        let out = block(
            &october_first_half(),
            date(2026, 10, 8),
            &portal,
            &plan,
            &inveniam(0.5),
        );
        assert_eq!(
            out,
            vec![
                String::new(),
                "**Период 1–15 октября** — прошло 6 из 11 рабочих дней (55%), по Чт 8 октября"
                    .to_string(),
                "- 💵 Inveniam SOW #5 — 18.0 из 44.0 ч (41%) ████░┃░░░░░ ↗75% · 0.38 FTE из 0.5 \
                 · из них в плане 5.5 ч"
                    .to_string(),
            ]
        );
    }

    /// No allocation in the config: the hours and the FTE, and where to set one.
    #[test]
    fn without_an_allocation_the_block_says_where_to_set_one() {
        let portal = [portal_day(
            date(2026, 10, 1),
            vec![worklog("Inveniam SOW #5", "Billable", 12.0)],
        )];
        let out = block(
            &october_first_half(),
            date(2026, 10, 8),
            &portal,
            &plan(vec![], vec![]),
            &[],
        );
        assert_eq!(
            out[2],
            "- 💵 Inveniam SOW #5 — 12.0 ч · 0.25 FTE · плановой аллокации нет: allocations в \
             ~/.config/tt-devpro/config.yaml"
        );
    }

    /// An allocated project with nothing logged is still listed, and the first day of a
    /// period claims no pace.
    #[test]
    fn on_the_first_day_an_allocated_project_shows_no_pace() {
        let period = Period {
            start: date(2026, 10, 16),
            end: date(2026, 10, 31),
            expected_hours: 88.0,
        };
        let out = block(
            &period,
            date(2026, 10, 16),
            &[],
            &plan(vec![], vec![]),
            &inveniam(0.5),
        );
        assert_eq!(
            out[1],
            "**Период 16–31 октября** — прошло 1 из 11 рабочих дней (9%), по Пт 16 октября"
        );
        assert_eq!(
            out[2],
            "- 💵 Inveniam SOW #5 — 0.0 из 44.0 ч (0%) ░┃░░░░░░░░░ · 0.0 FTE из 0.5"
        );
    }

    #[test]
    fn a_period_with_no_billable_hours_and_no_allocation_says_so() {
        let out = block(
            &october_first_half(),
            date(2026, 10, 8),
            &[],
            &plan(vec![], vec![]),
            &[],
        );
        assert_eq!(out[2], "- 💵 billable-часов в периоде нет");
    }

    /// An unplanned day has no lines, so its hours are missing; the block says how many.
    #[test]
    fn days_that_could_not_be_planned_are_named_as_missing() {
        let errors = vec![
            DayError {
                date: date(2026, 10, 7),
                message: "unmapped".to_string(),
            },
            DayError {
                date: date(2026, 9, 30),
                message: "unmapped".to_string(),
            },
        ];
        let out = block(
            &october_first_half(),
            date(2026, 10, 8),
            &[],
            &plan(vec![], errors),
            &[],
        );
        assert_eq!(
            out.last().unwrap(),
            "- ⚠️ не спланировано 1 дн. в периоде — их плановые часы не учтены"
        );
    }

    #[test]
    fn the_bar_puts_the_tick_between_cells_and_off_both_ends() {
        assert_eq!(bar(0.41, true, 6.0 / 11.0), "████░┃░░░░░");
        assert_eq!(bar(1.5, true, 1.0), "█████████┃█");
        assert_eq!(bar(0.0, true, 0.0), "░┃░░░░░░░░░");
        assert_eq!(bar(0.3, false, 0.0), "███░░░░░░░");
    }

    #[test]
    fn a_period_across_two_months_names_both() {
        assert_eq!(
            period_name(date(2026, 10, 26), date(2026, 11, 10)),
            "26 октября – 10 ноября"
        );
    }
}
