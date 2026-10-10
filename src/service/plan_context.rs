//! What `settle` knows about each day before the model sees it.
//!
//! The command fetches — Chrono entries, `normalView` months, the projects assigned on each
//! day — and [`build`] turns that into a [`Context`] with no I/O of its own, so every rule
//! below is tested on literals.
//!
//! A day splits into two kinds of line. **Pinned** lines keep their title and hours whatever
//! the model says, because the model never receives them as something to answer: meetings
//! (an auditor reads a meeting's length off the calendar), lines an override caps with
//! `max_hours`, worklogs already in DevPro, and lines Yurii edited in `plan.md`. **Free**
//! lines are the Chrono work the model names and stretches. What the free lines cannot
//! plausibly fill, the model fills from the day's [`Candidate`]s.
//!
//! A day that cannot be planned becomes a [`DayError`] and the other days go on: an unmapped
//! Chrono project, a project that does not resolve on that date, a worklog off the quarter
//! grid, pinned lines past 8 h.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, anyhow};
use chrono::{Datelike, NaiveDate, TimeZone, Weekday};
use serde::Serialize;

use crate::commands::holidays::is_us_federal_holiday;
use crate::config::Config;
use crate::model::{
    ChronoTimeEntry, DayProjectAggregate, NormalViewResponse, Project, WorklogDetail,
};
use crate::plan::render::hours;
use crate::plan::{
    ChronoKey, DAY_QUARTERS, DayError, DayPlan, LineKind, PlanLine, Quarters, hours_to_quarters,
    quarters_to_hours,
};
use crate::service::aggregator::{self, Aggregated, FallbackId, UnmappedEntry};
use crate::service::normalizer::strip_date_suffix;

/// How far back the history candidates look: the working week before the day.
pub const HISTORY_WORKING_DAYS: usize = 5;

/// History candidates offered per day, the biggest first.
const HISTORY_CANDIDATES: usize = 8;

/// How far back the DevPro titles the model must not repeat go.
pub const RECENT_TITLE_DAYS: u64 = 30;

/// Recent titles passed to the model, the most recent first.
const RECENT_TITLES: usize = 80;

/// The cleaned task title of a Chrono description: a trailing `" - <chrono project>"` and
/// then a trailing `", Mon D YYYY"` removed, in that order — the calendar import appends
/// both, with the project outermost.
pub fn clean_task_title(description: &str, chrono_project: &str) -> String {
    let project_suffix = format!(" - {chrono_project}");
    let without_suffix = description
        .strip_suffix(&project_suffix)
        .unwrap_or(description);
    strip_date_suffix(without_suffix)
}

/// A title DevPro can take: non-empty, printable ASCII only. DevPro titles are English, and
/// a Cyrillic meeting name sent as it is would be the one Russian line in an English log.
pub fn is_plain_ascii(title: &str) -> bool {
    !title.trim().is_empty() && title.chars().all(|c| (' '..='~').contains(&c))
}

// ---------------------------------------------------------------------------
// The portal side
// ---------------------------------------------------------------------------

/// One day of `normalView`.
#[derive(Debug, Clone, PartialEq)]
pub struct PortalDay {
    pub date: NaiveDate,
    pub logged_hours: f64,
    pub worklogs: Vec<WorklogDetail>,
}

/// Every day the `normalView` months hold, by date. A date in two responses keeps the later.
pub fn portal_days(views: &[NormalViewResponse]) -> Result<Vec<PortalDay>> {
    let mut by_date: BTreeMap<NaiveDate, PortalDay> = BTreeMap::new();
    for view in views {
        for page in &view.page_list {
            for day in &page.details_by_dates {
                let date = detail_date(&day.date)?;
                by_date.insert(
                    date,
                    PortalDay {
                        date,
                        logged_hours: day.logged_hours,
                        worklogs: day.worklogs_details.clone(),
                    },
                );
            }
        }
    }
    Ok(by_date.into_values().collect())
}

/// The first day of every month the closed interval `[start, end]` touches. `normalView`
/// keys on the month, not the day: any date inside a month returns that whole month.
pub fn months_in_range(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut months = Vec::new();
    let mut current = start.with_day(1).expect("every month has a first day");
    while current <= end {
        months.push(current);
        current = next_month(current);
    }
    months
}

fn next_month(first_of_month: NaiveDate) -> NaiveDate {
    let (year, month) = if first_of_month.month() == 12 {
        (first_of_month.year() + 1, 1)
    } else {
        (first_of_month.year(), first_of_month.month() + 1)
    };
    NaiveDate::from_ymd_opt(year, month, 1).expect("the first of the next month is a date")
}

/// The date half of the portal's ISO timestamp.
fn detail_date(raw: &str) -> Result<NaiveDate> {
    let head: String = raw.chars().take(10).collect();
    NaiveDate::parse_from_str(&head, "%Y-%m-%d")
        .map_err(|_| anyhow!("the portal returned '{raw}', which does not start with a date"))
}

/// Monday to Friday and not a US federal holiday.
pub fn is_working_day(date: NaiveDate) -> bool {
    !matches!(date.weekday(), Weekday::Sat | Weekday::Sun) && !is_us_federal_holiday(date)
}

/// The `count` working days before `day`, latest first.
pub fn working_days_before(day: NaiveDate, count: usize) -> Vec<NaiveDate> {
    let mut days = Vec::with_capacity(count);
    let mut current = day;
    while days.len() < count {
        current = current
            .pred_opt()
            .expect("a date before any working day exists");
        if is_working_day(current) {
            days.push(current);
        }
    }
    days
}

// ---------------------------------------------------------------------------
// The context
// ---------------------------------------------------------------------------

/// A Chrono line the model names and sizes.
#[derive(Debug, Clone, PartialEq)]
pub struct ChronoLine {
    /// The id the model answers with, unique within the day.
    pub key: String,
    pub chrono: ChronoKey,
    pub entry_count: u32,
    pub chrono_hours: f64,
    pub devpro_project: String,
    pub project_id: String,
    pub billability: String,
}

impl ChronoLine {
    pub fn into_plan_line(self, title: String, needs_detail: bool, quarters: Quarters) -> PlanLine {
        PlanLine {
            addr: 0,
            kind: LineKind::Work,
            chrono: Some(self.chrono),
            entry_count: self.entry_count,
            chrono_hours: Some(self.chrono_hours),
            title,
            needs_detail,
            devpro_project: self.devpro_project,
            project_id: self.project_id,
            billability: self.billability,
            quarters,
            pinned: false,
            edited: false,
            worklog_id: None,
            candidate_id: None,
        }
    }
}

/// Where a candidate came from, which decides what it may become.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// Chrono work of the last working days: the main topic, or a borrowed task.
    History,
    /// A `fillers:` entry of the config: a filler line only.
    Filler,
}

/// Something the day may be filled with beyond its own Chrono lines.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub source: CandidateSource,
    /// The Chrono description, or the filler's `task_title`.
    pub topic: String,
    /// Hours Chrono logged on it over the last working days; `None` for a filler.
    pub recent_hours: Option<f64>,
    pub devpro_project: String,
    pub project_id: String,
    pub billability: String,
    /// A filler's own `max_hours`.
    pub max_quarters: Option<Quarters>,
}

/// One Chrono line of the day in time order, for expanding a vague description from what
/// happened around it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineItem {
    /// Local `HH:MM` of the first merged entry.
    pub start: String,
    pub chrono_project: String,
    pub description: String,
    pub hours: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DayContext {
    pub date: NaiveDate,
    /// Lines the model may not change, recorded worklogs included.
    pub pinned: Vec<PlanLine>,
    pub free: Vec<ChronoLine>,
    pub candidates: Vec<Candidate>,
    pub timeline: Vec<TimelineItem>,
}

impl DayContext {
    pub fn pinned_quarters(&self) -> Quarters {
        self.pinned.iter().map(|line| line.quarters).sum()
    }
}

/// A DevPro title of the last weeks, for the model to vary rather than repeat.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecentTitle {
    pub title: String,
    pub devpro_project: String,
    pub last_date: NaiveDate,
    pub days: u32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Context {
    /// Days the model plans.
    pub days: Vec<DayContext>,
    /// Days whose pinned lines already come to 8 h: planned without the model.
    pub ready: Vec<DayPlan>,
    pub errors: Vec<DayError>,
    /// Days DevPro already holds at 8 h.
    pub closed: Vec<NaiveDate>,
    pub recent_titles: Vec<RecentTitle>,
    /// Project names resolved from `project_ids` rather than the live list, each once.
    pub fallbacks: Vec<FallbackId>,
}

/// What a replan carries over for one day.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DayPins {
    /// Chrono lines with a fixed title and hours: edited now or in an earlier round.
    pub chrono: HashMap<ChronoKey, PlanLine>,
    /// Chrono lines Yurii removed from the plan, now or in an earlier round.
    pub removed: HashSet<ChronoKey>,
    /// Lines with no Chrono line behind them that stay as they are: added rows, and main,
    /// filler or borrowed lines Yurii edited.
    pub synthetic: Vec<PlanLine>,
}

/// Everything [`build`] reads.
pub struct Inputs<'a> {
    pub config: &'a Config,
    /// The days to plan.
    pub days: &'a [NaiveDate],
    /// The days were named with `--from`/`--to` rather than found by the scan: a working
    /// day with no Chrono work is then an error rather than a day to pass over.
    pub explicit: bool,
    /// Chrono entries from [`HISTORY_WORKING_DAYS`] working days before the first day on.
    pub entries: &'a [ChronoTimeEntry],
    /// `normalView` from [`RECENT_TITLE_DAYS`] before the first day on.
    pub portal: &'a [PortalDay],
    /// The projects assigned on each day to plan.
    pub assigned: &'a HashMap<NaiveDate, Vec<Project>>,
    pub pins: &'a HashMap<NaiveDate, DayPins>,
}

pub fn build<Tz, M>(inputs: &Inputs, is_meeting: M, zone: &Tz) -> Result<Context>
where
    Tz: TimeZone,
    M: Fn(&DayProjectAggregate) -> bool,
{
    let aggregation =
        aggregator::aggregate_in_zone(inputs.entries, inputs.config, None, None, zone)?;
    let mut lines_by_day: HashMap<NaiveDate, Vec<&Aggregated>> = HashMap::new();
    for line in &aggregation.lines {
        lines_by_day
            .entry(line.aggregate.date)
            .or_default()
            .push(line);
    }
    let mut unmapped_by_day: HashMap<NaiveDate, Vec<&UnmappedEntry>> = HashMap::new();
    for entry in &aggregation.unmapped {
        unmapped_by_day.entry(entry.date).or_default().push(entry);
    }
    let portal_by_day: HashMap<NaiveDate, &PortalDay> =
        inputs.portal.iter().map(|day| (day.date, day)).collect();

    let mut days: Vec<NaiveDate> = inputs.days.to_vec();
    days.sort();
    days.dedup();

    let mut context = Context {
        recent_titles: days
            .first()
            .map(|first| recent_titles(inputs.portal, *first))
            .unwrap_or_default(),
        ..Context::default()
    };
    let no_pins = DayPins::default();

    for date in days {
        let day = DayInputs {
            date,
            config: inputs.config,
            assigned: inputs
                .assigned
                .get(&date)
                .ok_or_else(|| anyhow!("no assigned projects were fetched for {date}"))?,
            lines: lines_by_day.get(&date).map(Vec::as_slice).unwrap_or(&[]),
            unmapped: unmapped_by_day.get(&date).map(Vec::as_slice).unwrap_or(&[]),
            portal: portal_by_day.get(&date).copied(),
            pins: inputs.pins.get(&date).unwrap_or(&no_pins),
            explicit: inputs.explicit,
        };
        match plan_day(
            &day,
            &aggregation.lines,
            &is_meeting,
            zone,
            &mut context.fallbacks,
        ) {
            Ok(Outcome::Model(day)) => context.days.push(day),
            Ok(Outcome::Ready(day)) => context.ready.push(day),
            Ok(Outcome::Closed) => context.closed.push(date),
            Ok(Outcome::Skip) => {}
            Err(message) => context.errors.push(DayError { date, message }),
        }
    }
    Ok(context)
}

struct DayInputs<'a> {
    date: NaiveDate,
    config: &'a Config,
    assigned: &'a [Project],
    lines: &'a [&'a Aggregated],
    unmapped: &'a [&'a UnmappedEntry],
    portal: Option<&'a PortalDay>,
    pins: &'a DayPins,
    explicit: bool,
}

enum Outcome {
    Model(DayContext),
    Ready(DayPlan),
    Closed,
    /// A day the scan found Chrono data on, none of it DevPro work: nothing to plan.
    Skip,
}

/// One day, or the reason it cannot be planned.
fn plan_day<Tz: TimeZone, M: Fn(&DayProjectAggregate) -> bool>(
    day: &DayInputs,
    history: &[Aggregated],
    is_meeting: &M,
    zone: &Tz,
    fallbacks: &mut Vec<FallbackId>,
) -> Result<Outcome, String> {
    let recorded = recorded_lines(day.portal)?;
    let recorded_quarters: Quarters = recorded.iter().map(|line| line.quarters).sum();
    if recorded_quarters >= DAY_QUARTERS {
        return Ok(Outcome::Closed);
    }
    if !day.unmapped.is_empty() {
        return Err(unmapped_message(day.unmapped, day.assigned));
    }
    if day.lines.is_empty() && day.pins.synthetic.is_empty() {
        if day.explicit && is_working_day(day.date) {
            return Err("в chrono нет записей DevPro - Work за этот день".to_string());
        }
        return Ok(Outcome::Skip);
    }

    let mut resolve = |name: &str| resolve_id(name, day.assigned, day.config, fallbacks);
    let mut pinned: Vec<PlanLine> = Vec::new();
    let mut free: Vec<ChronoLine> = Vec::new();
    for line in day.lines {
        let aggregate = &line.aggregate;
        let key = chrono_key(aggregate);
        if day.pins.removed.contains(&key) {
            continue;
        }
        if let Some(pin) = day.pins.chrono.get(&key) {
            pinned.push(PlanLine {
                entry_count: line.entry_count,
                chrono_hours: Some(aggregate.total_hours),
                pinned: true,
                ..pin.clone()
            });
            continue;
        }
        let project_id = resolve(&aggregate.devpro_project_name)?;
        let meeting = is_meeting(aggregate);
        if meeting || aggregate.max_hours.is_some() {
            let title = clean_task_title(&key.description, &key.project);
            pinned.push(PlanLine {
                addr: 0,
                kind: if meeting {
                    LineKind::Meeting
                } else {
                    LineKind::Work
                },
                needs_detail: !is_plain_ascii(&title),
                chrono: Some(key),
                entry_count: line.entry_count,
                chrono_hours: Some(aggregate.total_hours),
                title,
                devpro_project: aggregate.devpro_project_name.clone(),
                project_id,
                billability: aggregate.billability.clone(),
                quarters: nearest_quarters(aggregate.total_hours),
                pinned: true,
                edited: false,
                worklog_id: None,
                candidate_id: None,
            });
        } else {
            free.push(ChronoLine {
                key: format!("c{}", free.len() + 1),
                chrono: key,
                entry_count: line.entry_count,
                chrono_hours: aggregate.total_hours,
                devpro_project: aggregate.devpro_project_name.clone(),
                project_id,
                billability: aggregate.billability.clone(),
            });
        }
    }
    pinned.extend(day.pins.synthetic.iter().map(|line| PlanLine {
        pinned: true,
        ..line.clone()
    }));

    let edited: Quarters = pinned.iter().map(|line| line.quarters).sum();
    let total = edited + recorded_quarters;
    pinned.extend(recorded);
    if total > DAY_QUARTERS {
        return Err(format!(
            "закреплённые строки дают {} ч, больше 8: встречи, строки с max_hours и правки {} ч, \
             уже в DevPro {} ч",
            hours(quarters_to_hours(total)),
            hours(quarters_to_hours(edited)),
            hours(quarters_to_hours(recorded_quarters)),
        ));
    }
    if total == DAY_QUARTERS {
        if !free.is_empty() {
            let names: Vec<&str> = free
                .iter()
                .map(|line| line.chrono.description.as_str())
                .collect();
            return Err(format!(
                "закреплённые строки уже дают 8 ч, рабочим строкам места нет: {}",
                names.join("; ")
            ));
        }
        return Ok(Outcome::Ready(DayPlan {
            date: day.date,
            lines: pinned,
        }));
    }

    let candidates = candidates(day, history, is_meeting);
    Ok(Outcome::Model(DayContext {
        date: day.date,
        pinned,
        free,
        candidates,
        timeline: timeline(day.lines, zone),
    }))
}

fn chrono_key(aggregate: &DayProjectAggregate) -> ChronoKey {
    ChronoKey {
        project: aggregate.chrono_project.clone(),
        description: aggregate.descriptions.first().cloned().unwrap_or_default(),
    }
}

/// The nearest quarter, never zero: a five-minute meeting still took place.
fn nearest_quarters(hours: f64) -> Quarters {
    ((hours * 4.0).round() as Quarters).max(1)
}

/// The day's worklogs already in DevPro, or the error of the first one off the grid.
fn recorded_lines(portal: Option<&PortalDay>) -> Result<Vec<PlanLine>, String> {
    let Some(portal) = portal else {
        return Ok(Vec::new());
    };
    portal
        .worklogs
        .iter()
        .map(|worklog| {
            let quarters = hours_to_quarters(worklog.logged_hours).ok_or_else(|| {
                format!(
                    "строка в DevPro «{}» на {} ч не кратна 0.25, день не сойдётся к 8.0",
                    worklog.task_title, worklog.logged_hours
                )
            })?;
            Ok(PlanLine {
                addr: 0,
                kind: LineKind::Recorded,
                chrono: None,
                entry_count: 0,
                chrono_hours: None,
                title: worklog.task_title.clone(),
                needs_detail: false,
                devpro_project: worklog.project_short_name.clone(),
                project_id: worklog.project_unique_id.clone(),
                billability: worklog.billability.clone(),
                quarters,
                pinned: true,
                edited: false,
                worklog_id: Some(worklog.unique_id.clone()),
                candidate_id: None,
            })
        })
        .collect()
}

/// The project id `name` resolves to on the day, or why it does not.
fn resolve_id(
    name: &str,
    assigned: &[Project],
    config: &Config,
    fallbacks: &mut Vec<FallbackId>,
) -> Result<String, String> {
    let resolution =
        aggregator::resolve_project_ids(&[name.to_string()], assigned, &config.project_ids)
            .map_err(|error| error.to_string())?;
    for fallback in resolution.fallbacks {
        if !fallbacks.contains(&fallback) {
            fallbacks.push(fallback);
        }
    }
    resolution
        .ids_by_name
        .get(name)
        .cloned()
        .ok_or_else(|| format!("DevPro project '{name}' not found"))
}

fn unmapped_message(unmapped: &[&UnmappedEntry], assigned: &[Project]) -> String {
    let mut by_project: Vec<(&str, Vec<String>)> = Vec::new();
    for entry in unmapped {
        let item = format!("{} {} ч", entry.description, hours(entry.hours));
        match by_project
            .iter_mut()
            .find(|(project, _)| *project == entry.chrono_project)
        {
            Some((_, items)) => items.push(item),
            None => by_project.push((&entry.chrono_project, vec![item])),
        }
    }
    let projects: Vec<String> = by_project
        .iter()
        .map(|(project, items)| format!("«{project}» ({})", items.join("; ")))
        .collect();
    let mut names: Vec<&str> = assigned.iter().map(|p| p.short_name.as_str()).collect();
    names.sort_unstable();
    format!(
        "chrono-проект без маппинга: {}. Назначены на дату: {}. Добавить: tt-devpro mapping add",
        projects.join(", "),
        names.join(", ")
    )
}

/// The day's candidates: the biggest Chrono work of the working week before it, then the
/// configured fillers, each only when its project is assigned on the day.
///
/// A candidate's project resolves against the day's assigned projects alone. The
/// `project_ids` fallback is for a Chrono line whose project was renamed; a synthetic line
/// has no Chrono work behind it, so a project not assigned on the day is no option for it.
fn candidates<M: Fn(&DayProjectAggregate) -> bool>(
    day: &DayInputs,
    history: &[Aggregated],
    is_meeting: &M,
) -> Vec<Candidate> {
    let assigned_id = |name: &str| {
        day.assigned
            .iter()
            .find(|project| project.short_name == name)
            .map(|project| project.unique_id.clone())
    };
    let window: HashSet<NaiveDate> = working_days_before(day.date, HISTORY_WORKING_DAYS)
        .into_iter()
        .collect();
    let mut groups: Vec<(ChronoKey, f64, &DayProjectAggregate)> = Vec::new();
    for line in history {
        let aggregate = &line.aggregate;
        if !window.contains(&aggregate.date)
            || aggregate.max_hours.is_some()
            || aggregate.descriptions.is_empty()
            || is_meeting(aggregate)
        {
            continue;
        }
        let key = chrono_key(aggregate);
        match groups.iter_mut().find(|(known, _, _)| *known == key) {
            Some((_, hours, _)) => *hours += aggregate.total_hours,
            None => groups.push((key, aggregate.total_hours, aggregate)),
        }
    }
    groups.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut out = Vec::new();
    for (key, recent, aggregate) in groups {
        if out.len() == HISTORY_CANDIDATES {
            break;
        }
        let Some(project_id) = assigned_id(&aggregate.devpro_project_name) else {
            continue;
        };
        out.push(Candidate {
            id: format!("h{}", out.len() + 1),
            source: CandidateSource::History,
            topic: clean_task_title(&key.description, &key.project),
            recent_hours: Some(recent),
            devpro_project: aggregate.devpro_project_name.clone(),
            project_id,
            billability: aggregate.billability.clone(),
            max_quarters: None,
        });
    }

    let mut fillers = 0;
    for filler in &day.config.fillers {
        let Some(project_id) = assigned_id(&filler.devpro_project) else {
            continue;
        };
        fillers += 1;
        out.push(Candidate {
            id: format!("f{fillers}"),
            source: CandidateSource::Filler,
            topic: filler.task_title.clone(),
            recent_hours: None,
            devpro_project: filler.devpro_project.clone(),
            project_id,
            billability: filler.billability.clone(),
            max_quarters: Some((filler.max_hours * 4.0).floor() as Quarters),
        });
    }
    out
}

fn timeline<Tz: TimeZone>(lines: &[&Aggregated], zone: &Tz) -> Vec<TimelineItem> {
    let mut ordered: Vec<&&Aggregated> = lines.iter().collect();
    ordered.sort_by_key(|line| line.first_start);
    ordered
        .into_iter()
        .map(|line| TimelineItem {
            start: line
                .first_start
                .with_timezone(zone)
                .naive_local()
                .format("%H:%M")
                .to_string(),
            chrono_project: line.aggregate.chrono_project.clone(),
            description: line
                .aggregate
                .descriptions
                .first()
                .cloned()
                .unwrap_or_default(),
            hours: line.aggregate.total_hours,
        })
        .collect()
}

/// Distinct DevPro titles of the [`RECENT_TITLE_DAYS`] before `first`, the most recent first.
fn recent_titles(portal: &[PortalDay], first: NaiveDate) -> Vec<RecentTitle> {
    let since = first - chrono::Days::new(RECENT_TITLE_DAYS);
    let mut titles: Vec<RecentTitle> = Vec::new();
    for day in portal
        .iter()
        .filter(|day| day.date >= since && day.date < first)
    {
        for worklog in &day.worklogs {
            match titles.iter_mut().find(|known| {
                known.title == worklog.task_title
                    && known.devpro_project == worklog.project_short_name
            }) {
                Some(known) => {
                    known.days += 1;
                    known.last_date = known.last_date.max(day.date);
                }
                None => titles.push(RecentTitle {
                    title: worklog.task_title.clone(),
                    devpro_project: worklog.project_short_name.clone(),
                    last_date: day.date,
                    days: 1,
                }),
            }
        }
    }
    titles.sort_by(|a, b| {
        b.last_date
            .cmp(&a.last_date)
            .then_with(|| b.days.cmp(&a.days))
    });
    titles.truncate(RECENT_TITLES);
    titles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Filler, OverrideRule, ProjectMapping};
    use crate::model::{ChronoProject, DateDetails, PageItem};
    use crate::plan::fixtures::date;
    use chrono::FixedOffset;

    const INVENIAM: &str = "Inveniam Measurabl - Presales - DevPro - Work";
    const AI: &str = "AI - DevPro - Work";

    fn config() -> Config {
        Config {
            chrono_api: "http://localhost:9247".to_string(),
            mappings: vec![
                ProjectMapping {
                    chrono_project: INVENIAM.to_string(),
                    devpro_project: "Inveniam SOW #5".to_string(),
                    billability: "Billable".to_string(),
                },
                ProjectMapping {
                    chrono_project: AI.to_string(),
                    devpro_project: "AI Practices".to_string(),
                    billability: "NonBillable".to_string(),
                },
            ],
            fillers: vec![Filler {
                devpro_project: "AI Practices".to_string(),
                task_title: "AI research".to_string(),
                billability: "NonBillable".to_string(),
                min_hours: 0.5,
                max_hours: 1.5,
            }],
            overrides: vec![OverrideRule {
                pattern: "Scorecard".to_string(),
                devpro_project: "AI Practices".to_string(),
                billability: "NonBillable".to_string(),
                max_hours: Some(0.5),
            }],
            project_ids: HashMap::new(),
            max_synthetic_hours: 4.0,
            plan_model: "sonnet".to_string(),
            vault_path: "/vault".into(),
            session_cookie: None,
            allocations: Vec::new(),
        }
    }

    fn project(name: &str) -> Project {
        Project {
            unique_id: format!("id-{name}"),
            short_name: name.to_string(),
            is_internal: false,
            is_favorite: false,
        }
    }

    fn assigned_on(days: &[NaiveDate]) -> HashMap<NaiveDate, Vec<Project>> {
        days.iter()
            .map(|day| {
                (
                    *day,
                    vec![project("Inveniam SOW #5"), project("AI Practices")],
                )
            })
            .collect()
    }

    /// An entry of `hours` starting at `hh:00` UTC on `day`.
    fn entry(
        id: i64,
        day: NaiveDate,
        hh: u32,
        project: &str,
        description: &str,
        hours: f64,
    ) -> ChronoTimeEntry {
        ChronoTimeEntry {
            id,
            description: Some(description.to_string()),
            start_time: format!("{day}T{hh:02}") + ":00:00Z",
            end_time: None,
            duration: Some((hours * 3600.0) as i64),
            project: Some(ChronoProject {
                id: 1,
                name: project.to_string(),
                color: "#000".to_string(),
                aspect: None,
            }),
            aspect: None,
        }
    }

    fn worklog(id: &str, title: &str, project: &str, hours: f64) -> WorklogDetail {
        WorklogDetail {
            unique_id: id.to_string(),
            project_unique_id: format!("id-{project}"),
            project_short_name: project.to_string(),
            task_title: title.to_string(),
            billability: "NonBillable".to_string(),
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

    fn meetings_named<'a>(names: &'a [&'a str]) -> impl Fn(&DayProjectAggregate) -> bool + 'a {
        move |aggregate| {
            aggregate
                .descriptions
                .first()
                .is_some_and(|d| names.contains(&d.as_str()))
        }
    }

    fn run(
        days: &[NaiveDate],
        explicit: bool,
        entries: &[ChronoTimeEntry],
        portal: &[PortalDay],
        pins: &HashMap<NaiveDate, DayPins>,
        meetings: &[&str],
    ) -> Context {
        let config = config();
        let assigned = assigned_on(days);
        let inputs = Inputs {
            config: &config,
            days,
            explicit,
            entries,
            portal,
            assigned: &assigned,
            pins,
        };
        build(
            &inputs,
            meetings_named(meetings),
            &FixedOffset::east_opt(0).unwrap(),
        )
        .unwrap()
    }

    fn monday() -> NaiveDate {
        date(2026, 10, 5)
    }

    #[test]
    fn meetings_and_capped_lines_are_pinned_and_work_is_free() {
        let day = monday();
        let entries = [
            entry(1, day, 9, INVENIAM, "D3 - Agentic DQ - Sync", 1.0),
            entry(2, day, 10, INVENIAM, "Map every Connect issue", 1.25),
            entry(3, day, 11, AI, "Scorecard for a candidate", 0.8),
            entry(4, day, 12, INVENIAM, "Map every Connect issue", 0.5),
        ];
        let context = run(
            &[day],
            false,
            &entries,
            &[],
            &HashMap::new(),
            &["D3 - Agentic DQ - Sync"],
        );
        let planned = &context.days[0];

        let pinned: Vec<(LineKind, &str, Quarters)> = planned
            .pinned
            .iter()
            .map(|l| (l.kind, l.title.as_str(), l.quarters))
            .collect();
        assert_eq!(
            pinned,
            vec![
                (LineKind::Work, "Scorecard for a candidate", 2),
                (LineKind::Meeting, "D3 - Agentic DQ - Sync", 4),
            ]
        );
        assert_eq!(planned.free.len(), 1);
        assert_eq!(planned.free[0].key, "c1");
        assert_eq!(planned.free[0].entry_count, 2);
        assert_eq!(planned.free[0].chrono_hours, 1.75);
        assert_eq!(planned.free[0].project_id, "id-Inveniam SOW #5");
        let starts: Vec<&str> = planned.timeline.iter().map(|t| t.start.as_str()).collect();
        assert_eq!(starts, vec!["09:00", "10:00", "11:00"]);
    }

    #[test]
    fn a_short_meeting_still_takes_a_quarter_and_a_cyrillic_title_needs_detail() {
        let day = monday();
        let entries = [entry(1, day, 9, AI, "Созвон с Ириной", 0.05)];
        let context = run(
            &[day],
            false,
            &entries,
            &[],
            &HashMap::new(),
            &["Созвон с Ириной"],
        );
        let line = &context.days[0].pinned[0];
        assert_eq!(line.quarters, 1);
        assert!(line.needs_detail);
    }

    #[test]
    fn an_unmapped_project_fails_its_day_and_names_what_is_assigned() {
        let day = monday();
        let other = date(2026, 10, 6);
        let entries = [
            entry(1, day, 9, "Nordis - DevPro - Work", "Estimate", 2.0),
            entry(2, other, 9, AI, "Cost framework", 2.0),
        ];
        let context = run(&[day, other], false, &entries, &[], &HashMap::new(), &[]);
        assert_eq!(context.errors.len(), 1);
        assert_eq!(context.errors[0].date, day);
        let message = &context.errors[0].message;
        assert!(
            message.contains("«Nordis - DevPro - Work» (Estimate 2.0 ч)"),
            "{message}"
        );
        assert!(
            message.contains("AI Practices, Inveniam SOW #5"),
            "{message}"
        );
        assert_eq!(context.days.len(), 1);
        assert_eq!(context.days[0].date, other);
    }

    #[test]
    fn an_unmapped_project_on_a_history_day_does_not_fail_the_plan() {
        let day = monday();
        let friday = date(2026, 10, 2);
        let entries = [
            entry(1, friday, 9, "Nordis - DevPro - Work", "Estimate", 2.0),
            entry(2, day, 9, AI, "Cost framework", 2.0),
        ];
        let context = run(&[day], false, &entries, &[], &HashMap::new(), &[]);
        assert!(context.errors.is_empty());
        assert_eq!(context.days.len(), 1);
    }

    #[test]
    fn recorded_worklogs_join_the_pinned_lines() {
        let day = monday();
        let entries = [entry(1, day, 9, AI, "Cost framework", 2.0)];
        let portal = [portal_day(
            day,
            vec![worklog("w-1", "Interview", "AI Practices", 1.5)],
        )];
        let context = run(&[day], false, &entries, &portal, &HashMap::new(), &[]);
        let recorded = &context.days[0].pinned[0];
        assert_eq!(recorded.kind, LineKind::Recorded);
        assert_eq!(recorded.worklog_id.as_deref(), Some("w-1"));
        assert_eq!(recorded.quarters, 6);
        assert_eq!(context.days[0].pinned_quarters(), 6);
    }

    #[test]
    fn a_recorded_worklog_off_the_grid_fails_its_day() {
        let day = monday();
        let entries = [entry(1, day, 9, AI, "Cost framework", 2.0)];
        let portal = [portal_day(
            day,
            vec![worklog("w-1", "Interview", "AI Practices", 1.1)],
        )];
        let context = run(&[day], false, &entries, &portal, &HashMap::new(), &[]);
        assert!(
            context.errors[0].message.contains("не кратна 0.25"),
            "{:?}",
            context.errors
        );
    }

    #[test]
    fn a_day_devpro_already_holds_at_eight_hours_is_closed() {
        let day = monday();
        let portal = [portal_day(
            day,
            vec![worklog("w-1", "Work", "AI Practices", 8.0)],
        )];
        let context = run(&[day], true, &[], &portal, &HashMap::new(), &[]);
        assert_eq!(context.closed, vec![day]);
        assert!(context.errors.is_empty() && context.days.is_empty());
    }

    #[test]
    fn a_named_working_day_with_no_chrono_work_is_an_error_and_a_weekend_is_skipped() {
        let saturday = date(2026, 10, 3);
        let context = run(&[saturday, monday()], true, &[], &[], &HashMap::new(), &[]);
        assert_eq!(context.errors.len(), 1);
        assert_eq!(context.errors[0].date, monday());

        let scanned = run(&[monday()], false, &[], &[], &HashMap::new(), &[]);
        assert!(scanned.errors.is_empty() && scanned.days.is_empty());
    }

    #[test]
    fn pinned_lines_past_eight_hours_fail_the_day_with_the_figures() {
        let day = monday();
        let entries = [
            entry(1, day, 9, AI, "Offsite", 7.0),
            entry(2, day, 16, AI, "Notes", 1.0),
        ];
        let portal = [portal_day(
            day,
            vec![worklog("w-1", "Interview", "AI Practices", 1.5)],
        )];
        let context = run(
            &[day],
            false,
            &entries,
            &portal,
            &HashMap::new(),
            &["Offsite"],
        );
        let message = &context.errors[0].message;
        assert!(message.contains("8.5 ч"), "{message}");
        assert!(message.contains("7.0 ч"), "{message}");
        assert!(message.contains("1.5 ч"), "{message}");
    }

    #[test]
    fn pinned_lines_at_exactly_eight_hours_plan_without_the_model_or_fail_with_free_work() {
        let day = monday();
        let full = [entry(1, day, 9, AI, "Offsite", 8.0)];
        let context = run(&[day], false, &full, &[], &HashMap::new(), &["Offsite"]);
        assert_eq!(context.ready.len(), 1);
        assert!(context.days.is_empty());

        let crowded = [
            entry(1, day, 9, AI, "Offsite", 8.0),
            entry(2, day, 17, AI, "Notes", 0.5),
        ];
        let context = run(&[day], false, &crowded, &[], &HashMap::new(), &["Offsite"]);
        assert!(
            context.errors[0].message.contains("Notes"),
            "{:?}",
            context.errors
        );
    }

    #[test]
    fn replan_pins_replace_removed_and_add_lines() {
        let day = monday();
        let entries = [
            entry(1, day, 9, AI, "Cost framework", 2.0),
            entry(2, day, 10, AI, "Notes", 1.0),
            entry(3, day, 11, INVENIAM, "Connect mapping", 1.0),
        ];
        let key = |description: &str, project: &str| ChronoKey {
            project: project.to_string(),
            description: description.to_string(),
        };
        let mut edited =
            crate::plan::fixtures::line(LineKind::Work, "AI cost model", "AI Practices", false, 12);
        edited.chrono = Some(key("Cost framework", AI));
        let added =
            crate::plan::fixtures::line(LineKind::Manual, "Hiring sync", "AI Practices", false, 2);
        let pins = HashMap::from([(
            day,
            DayPins {
                chrono: HashMap::from([(key("Cost framework", AI), edited)]),
                removed: HashSet::from([key("Notes", AI)]),
                synthetic: vec![added],
            },
        )]);
        let context = run(&[day], false, &entries, &[], &pins, &[]);
        let planned = &context.days[0];
        let pinned: Vec<(&str, Quarters, bool)> = planned
            .pinned
            .iter()
            .map(|l| (l.title.as_str(), l.quarters, l.pinned))
            .collect();
        assert_eq!(
            pinned,
            vec![("AI cost model", 12, true), ("Hiring sync", 2, true)]
        );
        assert_eq!(planned.pinned[0].chrono_hours, Some(2.0));
        let free: Vec<&str> = planned
            .free
            .iter()
            .map(|l| l.chrono.description.as_str())
            .collect();
        assert_eq!(free, vec!["Connect mapping"]);
    }

    #[test]
    fn candidates_come_from_the_working_week_before_and_the_fillers() {
        let day = monday();
        let entries = [
            // Friday and Thursday: history.
            entry(1, date(2026, 10, 2), 9, AI, "Cost framework", 3.0),
            entry(2, date(2026, 10, 1), 9, AI, "Cost framework", 2.0),
            entry(3, date(2026, 10, 1), 13, INVENIAM, "Connect mapping", 4.0),
            entry(4, date(2026, 10, 1), 15, INVENIAM, "Weekly sync", 1.0),
            // A Saturday is no working day, so it is not read.
            entry(5, date(2026, 10, 3), 9, AI, "Weekend reading", 9.0),
            // Eight working days back: outside the window.
            entry(6, date(2026, 9, 23), 9, AI, "Old topic", 9.0),
            entry(7, day, 9, AI, "Notes", 1.0),
        ];
        let context = run(
            &[day],
            false,
            &entries,
            &[],
            &HashMap::new(),
            &["Weekly sync"],
        );
        let candidates: Vec<(&str, CandidateSource, &str, Option<f64>)> = context.days[0]
            .candidates
            .iter()
            .map(|c| (c.id.as_str(), c.source, c.topic.as_str(), c.recent_hours))
            .collect();
        assert_eq!(
            candidates,
            vec![
                ("h1", CandidateSource::History, "Cost framework", Some(5.0)),
                ("h2", CandidateSource::History, "Connect mapping", Some(4.0)),
                ("f1", CandidateSource::Filler, "AI research", None),
            ]
        );
        assert_eq!(context.days[0].candidates[2].max_quarters, Some(6));
    }

    #[test]
    fn a_candidate_whose_project_is_not_assigned_on_the_day_is_left_out() {
        let day = monday();
        let entries = [
            entry(1, date(2026, 10, 2), 9, INVENIAM, "Connect mapping", 4.0),
            entry(2, day, 9, AI, "Notes", 1.0),
        ];
        // A fallback id for each unassigned project: a synthetic line must not reach for it.
        let mut config = config();
        config.fillers.push(Filler {
            devpro_project: "Artory".to_string(),
            ..config.fillers[0].clone()
        });
        config.project_ids = HashMap::from([
            ("Inveniam SOW #5".to_string(), "id-inv".to_string()),
            ("Artory".to_string(), "id-artory".to_string()),
        ]);
        let assigned = HashMap::from([(day, vec![project("AI Practices")])]);
        let pins = HashMap::new();
        let inputs = Inputs {
            config: &config,
            days: &[day],
            explicit: false,
            entries: &entries,
            portal: &[],
            assigned: &assigned,
            pins: &pins,
        };
        let context = build(
            &inputs,
            |_: &DayProjectAggregate| false,
            &FixedOffset::east_opt(0).unwrap(),
        )
        .unwrap();
        let projects: Vec<&str> = context.days[0]
            .candidates
            .iter()
            .map(|c| c.devpro_project.as_str())
            .collect();
        assert_eq!(projects, ["AI Practices"]);
        assert!(context.fallbacks.is_empty(), "{:?}", context.fallbacks);
    }

    #[test]
    fn recent_titles_cover_the_thirty_days_before_the_first_day_most_recent_first() {
        let day = monday();
        let portal = [
            portal_day(
                date(2026, 8, 20),
                vec![worklog("a", "Too old", "AI Practices", 8.0)],
            ),
            portal_day(
                date(2026, 9, 29),
                vec![worklog("b", "Cost model", "AI Practices", 8.0)],
            ),
            portal_day(
                date(2026, 10, 1),
                vec![worklog("c", "Cost model", "AI Practices", 8.0)],
            ),
            portal_day(
                date(2026, 10, 2),
                vec![worklog("d", "Connect", "AI Practices", 8.0)],
            ),
        ];
        let titles = recent_titles(&portal, day);
        let got: Vec<(&str, u32)> = titles.iter().map(|t| (t.title.as_str(), t.days)).collect();
        assert_eq!(got, vec![("Connect", 1), ("Cost model", 2)]);
    }

    #[test]
    fn working_days_before_skip_weekends_and_holidays() {
        // 2026-09-07 is Labor Day, a Monday.
        let days = working_days_before(date(2026, 9, 8), 3);
        assert_eq!(
            days,
            vec![date(2026, 9, 4), date(2026, 9, 3), date(2026, 9, 2)]
        );
    }

    #[test]
    fn portal_days_keep_the_worklogs_of_every_month() {
        let view = |day: &str, title: &str| NormalViewResponse {
            total_logged_hours: 8.0,
            total_expected_hours: 8.0,
            page_list: vec![PageItem {
                contact_unique_id: "me".to_string(),
                full_name: "Me".to_string(),
                logged_hours: 8.0,
                expected_hours: 8.0,
                details_by_dates: vec![DateDetails {
                    date: format!("{day}T00:00:00"),
                    logged_hours: 8.0,
                    expected_hours: 8.0,
                    worklogs_details: vec![worklog("w", title, "AI Practices", 8.0)],
                }],
            }],
        };
        let days = portal_days(&[view("2026-10-01", "Oct"), view("2026-09-30", "Sep")]).unwrap();
        let got: Vec<(NaiveDate, &str)> = days
            .iter()
            .map(|d| (d.date, d.worklogs[0].task_title.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![(date(2026, 9, 30), "Sep"), (date(2026, 10, 1), "Oct")]
        );
        assert!(portal_days(&[view("2026-1", "x")]).is_err());
    }

    #[test]
    fn months_in_range_is_inclusive_and_crosses_the_year() {
        assert_eq!(
            months_in_range(date(2026, 11, 20), date(2027, 1, 1)),
            vec![date(2026, 11, 1), date(2026, 12, 1), date(2027, 1, 1)]
        );
    }

    #[test]
    fn a_title_loses_its_project_suffix_then_its_date() {
        assert_eq!(
            clean_task_title("Event, Apr 8 2026 - Some Project", "Some Project"),
            "Event"
        );
        assert_eq!(
            clean_task_title("Wrote docs - OtherProj", "MyProj"),
            "Wrote docs - OtherProj"
        );
        assert_eq!(clean_task_title("", "Some Project"), "");
    }

    #[test]
    fn plain_ascii_titles_are_printable_and_not_blank() {
        assert!(is_plain_ascii("AI cost model v2"));
        assert!(!is_plain_ascii("Созвон"));
        assert!(!is_plain_ascii("  "));
        assert!(!is_plain_ascii("tab\there"));
    }
}
