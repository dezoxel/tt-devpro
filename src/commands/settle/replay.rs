//! A plan for days already settled, set beside what was actually written to DevPro.
//!
//! The planner's acceptance test: it plans a past range as if nothing had been written yet
//! and prints the plan next to Yurii's own worklogs for the same days. DevPro is read and
//! never written, and the stored plan is not touched, so a replay can run at any time.
//!
//! What the planner sees is cut at `from`. The DevPro titles it may vary come only from the
//! days before the range, and the days inside it enter with nothing recorded, so the answer
//! is not in the input.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::*;

/// Hours by DevPro project on one day, planned against written, differing by more than this
/// count as a miss.
const HOURS_TOLERANCE: f64 = 0.25;

/// Plans `from..=to` with the live config, Chrono, DevPro and model, and returns the plan
/// followed by the comparison with DevPro, as markdown. `model` replaces the config's
/// `plan_model` for this run, to compare models on the same days.
pub async fn replay(from: NaiveDate, to: NaiveDate, model: Option<String>) -> Result<String> {
    if from > to {
        bail!("--from {from} is after --to {to}");
    }
    let config = crate::config::load()?;
    let state = State::locate()?;
    let chrono = ChronoClient::new(&config.chrono_api)?;
    let portal = TtApiClient::new(crate::cookie::session_cookie()?)?;
    let settle = Settle {
        config: &config,
        chrono: &chrono,
        portal: &portal,
        state: &state,
        zone: &Local,
        today: Local::now().date_naive(),
        now: Utc::now,
    };
    let normalizer = TimeNormalizer::for_settle(&config.vault_path)?;
    let model = ClaudeCliModel::new(model.unwrap_or_else(|| config.plan_model.clone()));
    let planning = Planning {
        model: &model,
        is_meeting: |aggregate: &DayProjectAggregate| normalizer.is_meeting_entry(aggregate),
    };
    let mut io = Stderr;
    settle.replay(from, to, &planning, &mut io).await
}

/// Warnings the planner prints go to stderr, so stdout holds the report alone.
struct Stderr;

impl Console for Stderr {
    fn out(&mut self, line: &str) {
        eprintln!("{line}");
    }
    fn err(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

impl<Tz: TimeZone> Settle<'_, Tz> {
    async fn replay<M, P>(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        planning: &Planning<'_, M, P>,
        io: &mut dyn Console,
    ) -> Result<String>
    where
        M: PlanModel,
        P: Fn(&DayProjectAggregate) -> bool,
    {
        let user = self.portal.get_current_user().await?;
        let entries = self.chrono_entries(from, to).await?;
        let (history, written): (Vec<PortalDay>, Vec<PortalDay>) = self
            .read_portal(recent_titles_start(from)?, to)
            .await?
            .into_iter()
            .partition(|day| day.date < from);
        let days: Vec<NaiveDate> = from.iter_days().take_while(|day| *day <= to).collect();
        let assigned = self.assigned(&user.unique_id, &days).await?;

        let planned = self
            .plan_days(
                planning,
                DaysToPlan {
                    days: &days,
                    explicit: true,
                    entries: &entries,
                    portal: &history,
                    assigned: &assigned,
                    pins: &HashMap::new(),
                },
                io,
            )
            .await?;
        let mut plan = Plan {
            prefix: String::new(),
            cutoff: to,
            days: planned.days,
            errors: planned.errors,
            closed: planned.closed,
            removed: BTreeMap::new(),
        };
        plan.number();

        let mut report = render(&plan);
        report.push_str("\n\n");
        report.push_str(&comparison(&plan, &written));
        Ok(report)
    }
}

/// Per day: hours by DevPro project, planned against written, then both title lists.
///
/// Lines are compared by project rather than one to one, because a planned title and the
/// title Yurii wrote for the same work are different strings by design.
fn comparison(plan: &Plan, written: &[PortalDay]) -> String {
    let mut dates: Vec<NaiveDate> = plan.days.iter().map(|day| day.date).collect();
    dates.extend(
        written
            .iter()
            .filter(|day| !day.worklogs.is_empty())
            .map(|day| day.date),
    );
    dates.sort();
    dates.dedup();

    let mut out = String::from("# Сверка с DevPro\n");
    let (mut pairs, mut misses) = (0, 0);
    for date in dates {
        let planned_day = plan.days.iter().find(|day| day.date == date);
        let written_day = written.iter().find(|day| day.date == date);

        let mut by_project: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
        for line in planned_day.iter().flat_map(|day| &day.lines) {
            by_project.entry(&line.devpro_project).or_default().0 += line.quarters as f64 / 4.0;
        }
        for worklog in written_day.iter().flat_map(|day| &day.worklogs) {
            by_project.entry(&worklog.project_short_name).or_default().1 += worklog.logged_hours;
        }

        let _ = writeln!(out, "\n**{}**\n", day_label(date));
        if planned_day.is_none() {
            let reason = plan
                .errors
                .iter()
                .find(|error| error.date == date)
                .map_or("no plan", |error| error.message.as_str());
            let _ = writeln!(out, "Not planned: {reason}\n");
        }
        for (project, (planned, actual)) in &by_project {
            pairs += 1;
            let miss = (planned - actual).abs() > HOURS_TOLERANCE;
            if miss {
                misses += 1;
            }
            let _ = writeln!(
                out,
                "- {project}: план {} · DevPro {}{}",
                hours(*planned),
                hours(*actual),
                if miss { " ⚠️" } else { "" }
            );
        }
        let _ = writeln!(out, "\nПлан:");
        for line in planned_day.iter().flat_map(|day| &day.lines) {
            let _ = writeln!(
                out,
                "- {} ({})",
                line.title,
                hours(line.quarters as f64 / 4.0)
            );
        }
        let _ = writeln!(out, "\nDevPro:");
        for worklog in written_day.iter().flat_map(|day| &day.worklogs) {
            let _ = writeln!(
                out,
                "- {} ({})",
                worklog.task_title,
                hours(worklog.logged_hours)
            );
        }
    }
    let _ = writeln!(
        out,
        "\nПроектов-дней с расхождением больше {} ч: {misses} из {pairs}",
        hours(HOURS_TOLERANCE)
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorklogDetail;
    use crate::plan::fixtures::date;

    fn written(day: NaiveDate, rows: &[(&str, &str, f64)]) -> PortalDay {
        PortalDay {
            date: day,
            logged_hours: rows.iter().map(|row| row.2).sum(),
            worklogs: rows
                .iter()
                .enumerate()
                .map(|(index, (project, title, hours))| WorklogDetail {
                    unique_id: format!("w{index}"),
                    project_unique_id: format!("id-{project}"),
                    project_short_name: project.to_string(),
                    task_title: title.to_string(),
                    billability: "Billable".to_string(),
                    logged_hours: *hours,
                    is_deletable: true,
                    expense_type: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_day_without_a_plan_is_listed_with_its_reason_and_counted_as_misses() {
        let day = date(2026, 10, 5);
        let plan = Plan {
            prefix: String::new(),
            cutoff: day,
            days: vec![],
            errors: vec![DayError {
                date: day,
                message: "Unmapped: X".to_string(),
            }],
            closed: vec![],
            removed: BTreeMap::new(),
        };
        let report = comparison(
            &plan,
            &[written(day, &[("AI Practices", "Cost model", 8.0)])],
        );
        assert!(report.contains("Not planned: Unmapped: X"), "{report}");
        assert!(
            report.contains("- AI Practices: план 0.0 · DevPro 8.0 ⚠️"),
            "{report}"
        );
        assert!(report.ends_with("больше 0.25 ч: 1 из 1\n"), "{report}");
    }
}
