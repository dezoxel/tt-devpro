//! The billing-period block `settle` prints under the plan: how far the current DevPro
//! period has gone and how much of the planned billable allocation it has used. Pure; the
//! reads are in `settle.rs`.
//!
//! The boundaries are the portal's own, from its PTR Periods view (`contact/ptrPeriods`),
//! never a 1–15 / 16–end rule: May 2026 ran 1–17 there. The allocation is not in the
//! portal at all, so it comes from `allocations` in the config.
//!
//! The pace reads like the weekly limit in the Claude Code statusline: a bar filled by the
//! share of the allocation used, a `┃` at the share that is due by now, and `↗N%`, the share
//! of the allocation the period ends at if the rate holds. The allocation is counted day by
//! day, so an FTE that changes mid-period moves the target, the `┃` and the pace with it.
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
    workdays(start, end).count() as u32
}

fn workdays(start: NaiveDate, end: NaiveDate) -> impl Iterator<Item = NaiveDate> {
    start
        .iter_days()
        .take_while(move |day| *day <= end)
        .filter(|day| !matches!(day.weekday(), Weekday::Sat | Weekday::Sun))
        .filter(|day| !is_us_federal_holiday(*day))
}

/// What the allocations ask of one project in a period, counted day by day.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    /// Hours over the whole period.
    pub allocated: f64,
    /// The share of `allocated` that falls on the working days up to `through`.
    pub due: f64,
    /// The mean fte over the working days up to `through`; `None` before the first of them
    /// an allocation holds.
    pub fte: Option<f64>,
}

/// The project's target in the period, or `None` when no entry holds any of its working days.
///
/// The portal's `expectedHours` stays the total, and each working day weighs its fte: a
/// period at one fte throughout is `fte × expectedHours`, as before entries had dates. The
/// days are the local predicate's, like the days gone; where the portal observes a holiday
/// the predicate does not, the weights are one day off, the same accepted drift.
pub fn target(
    allocations: &[Allocation],
    project: &str,
    period: &Period,
    through: NaiveDate,
) -> Option<Target> {
    let fte_on = |day: NaiveDate| {
        allocations
            .iter()
            .find(|a| a.devpro_project == project && a.holds(day))
            .map_or(0.0, |a| a.fte)
    };
    let (mut days, mut total, mut gone_days, mut gone) = (0u32, 0.0, 0u32, 0.0);
    for day in workdays(period.start, period.end) {
        let fte = fte_on(day);
        days += 1;
        total += fte;
        if day <= through {
            gone_days += 1;
            gone += fte;
        }
    }
    (total > 0.0).then(|| Target {
        allocated: period.expected_hours * total / f64::from(days),
        due: gone / total,
        fte: (gone > 0.0).then(|| gone / f64::from(gone_days)),
    })
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
    // An entry that holds none of the period's days has nothing to show in it.
    for allocation in allocations {
        if target(allocations, &allocation.devpro_project, period, through).is_some() {
            billable
                .entry(allocation.devpro_project.clone())
                .or_default();
        }
    }
    if billable.is_empty() {
        out.push(format!("- {BILLABLE_MARK} billable-часов в периоде нет"));
    }
    for (project, so_far) in &billable {
        out.push(project_line(
            project,
            *so_far,
            target(allocations, project, period, through),
            elapsed_days,
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
    target: Option<Target>,
    elapsed_days: u32,
) -> String {
    let logged = so_far.total();
    let fte = (elapsed_days > 0).then(|| logged / (8.0 * f64::from(elapsed_days)));
    let mut line = format!("- {BILLABLE_MARK} {project} — ");
    match target {
        Some(target) => {
            let used = logged / target.allocated;
            let due = target.due;
            line.push_str(&format!(
                "{} из {} ч ({}%) {}",
                hours(logged),
                hours(target.allocated),
                percent(used),
                bar(used, elapsed_days > 0 && due > 0.0, due)
            ));
            if elapsed_days >= MIN_DAYS_FOR_PACE && due > 0.0 {
                let pace = ((used / due * 100.0).round() as u32).min(MAX_PACE);
                line.push_str(&format!(" ↗{pace}%"));
            }
            match (fte, target.fte) {
                (Some(fte), Some(planned)) => {
                    line.push_str(&format!(" · {} FTE из {}", hours(fte), hours(planned)));
                }
                (Some(fte), None) => line.push_str(&format!(" · {} FTE", hours(fte))),
                (None, _) => {}
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
/// cells at the share due by now, kept off both ends where it would separate nothing.
fn bar(used: f64, started: bool, due: f64) -> String {
    let filled = ((used * f64::from(BAR_CELLS)).floor() as u32).min(BAR_CELLS);
    let tick =
        started.then(|| ((due * f64::from(BAR_CELLS)).round() as u32).clamp(1, BAR_CELLS - 1));
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

    fn allocation(fte: f64, from: NaiveDate, to: NaiveDate) -> Allocation {
        Allocation {
            devpro_project: "Inveniam SOW #5".to_string(),
            fte,
            from,
            to,
        }
    }

    /// One fte over all of October.
    fn inveniam(fte: f64) -> Vec<Allocation> {
        vec![allocation(fte, date(2026, 10, 1), date(2026, 10, 31))]
    }

    /// The allocation email of 2026-10-12: 0.5 up to 11 October, 0.75 from the 12th.
    fn inveniam_raised_on_the_12th() -> Vec<Allocation> {
        vec![
            allocation(0.5, date(2026, 8, 28), date(2026, 10, 11)),
            allocation(0.75, date(2026, 10, 12), date(2026, 12, 12)),
        ]
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

    // -- the allocation by day -----------------------------------------------

    /// 1–15 October 2026: 7 working days at 0.5 and 4 at 0.75 are 7 × 4 h + 4 × 6 h = 52 h,
    /// not the 66 h of 0.75 over the whole period.
    #[test]
    fn an_fte_raised_mid_period_counts_each_day_at_its_own_fte() {
        let target = target(
            &inveniam_raised_on_the_12th(),
            "Inveniam SOW #5",
            &october_first_half(),
            date(2026, 10, 13),
        )
        .unwrap();
        assert_eq!(target.allocated, 52.0);
        assert_eq!(target.due, 5.0 / 6.5);
        assert_eq!(target.fte, Some(5.0 / 9.0));
    }

    /// One fte over the whole period is the old `fte × expectedHours`, and the tick stands
    /// where the days gone put it.
    #[test]
    fn one_fte_over_the_period_is_fte_times_the_expected_hours() {
        let target = target(
            &inveniam(0.5),
            "Inveniam SOW #5",
            &october_first_half(),
            date(2026, 10, 8),
        )
        .unwrap();
        assert_eq!(target.allocated, 44.0);
        assert!((target.due - 6.0 / 11.0).abs() < 1e-12, "{}", target.due);
        assert_eq!(target.fte, Some(0.5));
    }

    /// Starting on the 12th with nothing before: only the four days from the 12th count.
    #[test]
    fn an_allocation_starting_mid_period_asks_only_for_its_own_days() {
        let allocations = [allocation(0.75, date(2026, 10, 12), date(2026, 12, 12))];
        let target = target(
            &allocations,
            "Inveniam SOW #5",
            &october_first_half(),
            date(2026, 10, 13),
        )
        .unwrap();
        assert_eq!(target.allocated, 24.0);
        assert_eq!(target.due, 0.5);
        assert_eq!(target.fte, Some(1.5 / 9.0));
    }

    #[test]
    fn an_allocation_outside_the_period_or_of_another_project_is_no_target() {
        let period = october_first_half();
        let ended = [allocation(0.5, date(2026, 8, 28), date(2026, 9, 30))];
        assert_eq!(
            target(&ended, "Inveniam SOW #5", &period, date(2026, 10, 8)),
            None
        );
        assert_eq!(
            target(&inveniam(0.5), "AI Practices", &period, date(2026, 10, 8)),
            None
        );
    }

    /// The morning of 14 October: 40 h of 52 is exactly what the days so far ask for, so the
    /// tick stands at 77% and the pace is 100%, at 0.56 of a planned 0.56 FTE.
    #[test]
    fn the_block_paces_a_raised_fte_against_what_is_due_so_far() {
        let portal = [portal_day(
            date(2026, 10, 1),
            vec![worklog("Inveniam SOW #5", "Billable", 40.0)],
        )];
        let out = block(
            &october_first_half(),
            date(2026, 10, 13),
            &portal,
            &plan(vec![], vec![]),
            &inveniam_raised_on_the_12th(),
        );
        assert_eq!(
            out[2],
            "- 💵 Inveniam SOW #5 — 40.0 из 52.0 ч (77%) ███████░┃░░ ↗100% · 0.56 FTE из 0.56"
        );
    }

    /// The entry starts after the last counted day: the target is known, nothing is due yet,
    /// so there is no tick, no pace and no planned FTE to compare with.
    #[test]
    fn before_the_allocation_starts_nothing_is_due() {
        let allocations = [allocation(0.75, date(2026, 10, 12), date(2026, 12, 12))];
        let portal = [portal_day(
            date(2026, 10, 1),
            vec![worklog("Inveniam SOW #5", "Billable", 4.0)],
        )];
        let out = block(
            &october_first_half(),
            date(2026, 10, 8),
            &portal,
            &plan(vec![], vec![]),
            &allocations,
        );
        assert_eq!(
            out[2],
            "- 💵 Inveniam SOW #5 — 4.0 из 24.0 ч (17%) █░░░░░░░░░ · 0.08 FTE"
        );
    }

    /// An entry that ended before the period is not listed; hours on the project are, with no
    /// allocation.
    #[test]
    fn an_ended_allocation_is_not_listed_in_a_later_period() {
        let ended = [allocation(0.5, date(2026, 8, 28), date(2026, 9, 30))];
        let empty = block(
            &october_first_half(),
            date(2026, 10, 8),
            &[],
            &plan(vec![], vec![]),
            &ended,
        );
        assert_eq!(empty[2], "- 💵 billable-часов в периоде нет");
        let portal = [portal_day(
            date(2026, 10, 1),
            vec![worklog("Inveniam SOW #5", "Billable", 12.0)],
        )];
        let out = block(
            &october_first_half(),
            date(2026, 10, 8),
            &portal,
            &plan(vec![], vec![]),
            &ended,
        );
        assert!(
            out[2].ends_with(
                "плановой аллокации нет: allocations в \
                                  ~/.config/tt-devpro/config.yaml"
            ),
            "{}",
            out[2]
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
