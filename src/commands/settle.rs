//! `tt-devpro settle`: the plan, the replan, and the one write.
//!
//! Three runs share this file, and only the last one writes to DevPro.
//!
//! - **`settle`** finds the days to plan, asks the plan model to name and spread the work
//!   lines, prints the plan as a markdown table and keeps it in [`State`]. It never writes.
//! - **`settle --replan`** reads the table back after the agent edited it on Yurii's word,
//!   pins every edited line, plans the edited days again around those pins, and prints the
//!   new table. Days without an edit are carried over untouched; days that failed last time
//!   are built again from the current config, which is how a mapping added in between
//!   reaches the plan.
//! - **`settle --apply`** writes the last table shown, and only that one: `plan.md` must still
//!   hash to what was printed. It creates worklogs and never updates one, because the
//!   portal's worklog listing does not carry every field an update would have to send back,
//!   and an update that blanks a field Yurii typed in the portal is worse than none. The one
//!   delete is its own undo: a write that fails mid-day removes the lines this run created on
//!   that day, so a day reaches DevPro whole or not at all.
//!
//! The window rules — which days are final, what the scan offers, what an explicit range
//! means — live in [`super::settle_window`] and are unchanged by the plan.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, Days, Local, NaiveDate, TimeZone};
use clap::Args;

use crate::api::chrono::ChronoClient;
use crate::api::portal::{ApiError, TtApiClient};
use crate::commands::settle_period;
use crate::commands::settle_window::{
    SCAN_DAYS, describe_not_final_days, last_settleable_day, nothing_to_settle_message,
    resolve_range, split_by_finality, unfilled_days,
};
use crate::commands::{Console, Outcome, parse_iso_date, usage_error, usage_line};
use crate::config::Config;
use crate::llm::{ClaudeCliModel, PlanModel};
use crate::model::{
    ChronoTimeEntry, CreateWorklogRequest, DayProjectAggregate, Project, WorklogDetail,
};
use crate::plan::parse::{self, DayEdits, Row};
use crate::plan::render::{day_label, hours, render};
use crate::plan::state::State;
use crate::plan::{
    BILLABLE, DAY_QUARTERS, DayError, DayPlan, LineKind, NON_BILLABLE, Plan, PlanLine, Quarters,
    hours_to_quarters, quarters_to_hours,
};
use crate::service::aggregator::{self, FallbackId, resolve_project_ids};
use crate::service::normalizer::TimeNormalizer;
use crate::service::plan_context::{
    self, DayPins, HISTORY_WORKING_DAYS, Inputs, PortalDay, RECENT_TITLE_DAYS, is_plain_ascii,
    months_in_range, working_days_before,
};
use crate::service::planner::{self, MAX_TITLE_CHARS};

// ---------------------------------------------------------------------------
// Argument surface
// ---------------------------------------------------------------------------

/// `--from` and `--to` convert during parsing, so a value that is not a date is a usage
/// failure rather than a runtime one — the same ordering `api get-projects --date` has.
#[derive(Args, Debug, Clone, Default)]
pub struct SettleArgs {
    /// Start date (YYYY-MM-DD), defaults to the 1st of this month.
    #[arg(long = "from", value_parser = parse_iso_date)]
    pub from: Option<NaiveDate>,

    /// End date (YYYY-MM-DD), defaults to the last completed day.
    #[arg(long = "to", value_parser = parse_iso_date)]
    pub to: Option<NaiveDate>,

    /// Also plan today, whose hours are not final.
    #[arg(long = "include-today")]
    pub include_today: bool,

    /// The letter in front of every line address.
    #[arg(long = "prefix")]
    pub prefix: Option<String>,

    /// Rebuild the plan from the edits made in plan.md.
    #[arg(long = "replan")]
    pub replan: bool,

    /// Write the plan last shown to DevPro.
    #[arg(long = "apply")]
    pub apply: bool,
}

/// `tt-devpro settle --help`, in the Clikt shape the rest of the CLI prints.
pub const SETTLE_HELP: &str = r#"Usage: tt-devpro settle [<options>]

  Plan worklogs from Chrono and show them; --apply writes the plan to DevPro

Options:
  --from=<value>   Start date (YYYY-MM-DD), defaults to the 1st of this month.
                   Without --from/--to scans the last 45 days for unfilled days
  --to=<value>     End date (YYYY-MM-DD), defaults to the last completed day.
                   Without --from/--to scans the last 45 days for unfilled days
  --include-today  Also plan today. Off by default: today is unfinished, so its
                   hours aren't final
  --prefix=<text>  Letter in front of every line address, e.g. Б
  --replan         Plan again from the edits made in the plan file
  --apply          Write the plan last shown to DevPro
  -h, --help       Show this message and exit"#;

/// `--replan` and `--apply` work on the stored plan, whose window and prefix were fixed when
/// it was built, so every option that shapes a new plan is refused next to them — silently
/// ignoring `--from` there would leave Yurii believing he applied a range he did not.
fn conflicts(args: &SettleArgs) -> Vec<String> {
    let mut messages = Vec::new();
    if args.replan && args.apply {
        messages.push("option --apply cannot be used with --replan".to_string());
    }
    let stored = if args.apply {
        "--apply"
    } else if args.replan {
        "--replan"
    } else {
        return messages;
    };
    let shaping = [
        (args.from.is_some(), "--from"),
        (args.to.is_some(), "--to"),
        (args.include_today, "--include-today"),
        (args.prefix.is_some(), "--prefix"),
    ];
    for (given, name) in shaping {
        if given {
            messages.push(format!("option {name} cannot be used with {stored}"));
        }
    }
    messages
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// `tt-devpro settle`. The message of a failure is already on stderr when this returns
/// [`Outcome::Failed`].
pub async fn run(args: &SettleArgs, io: &mut dyn Console) -> Outcome {
    let conflicts = conflicts(args);
    if !conflicts.is_empty() {
        io.err(&usage_error(usage_line(SETTLE_HELP), &conflicts));
        return Outcome::Failed;
    }

    let config = match crate::config::load() {
        Ok(config) => config,
        Err(error) => {
            io.err(&format!("\u{2717} {error:#}"));
            return Outcome::Failed;
        }
    };

    // Read once, so a run crossing midnight cannot disagree with itself about "today".
    let today = Local::now().date_naive();

    match dispatch(args, &config, today, io).await {
        Ok(outcome) => outcome,
        Err(error) => {
            report_failure(&error, io);
            Outcome::Failed
        }
    }
}

/// The portal's own errors print as `API Error`, everything else with its whole chain:
/// the outermost context alone would be `requesting <url>` with the refusal dropped.
fn report_failure(error: &anyhow::Error, io: &mut dyn Console) {
    match error.downcast_ref::<ApiError>() {
        Some(api) => io.err(&format!("\u{2717} API Error: {api}")),
        None => io.err(&format!("\u{2717} Error: {error:#}")),
    }
}

/// The live composition: real clients, the session cookie, the vault and the model.
///
/// `--apply` walks no vault and starts no model. It writes what was shown, so nothing it
/// does depends on either, and a vault that moved since the plan was built must not stop it.
async fn dispatch(
    args: &SettleArgs,
    config: &Config,
    today: NaiveDate,
    io: &mut dyn Console,
) -> Result<Outcome> {
    let state = State::locate()?;
    let chrono = ChronoClient::new(&config.chrono_api)?;
    let portal = TtApiClient::new(crate::cookie::session_cookie()?)?;
    let settle = Settle {
        config,
        chrono: &chrono,
        portal: &portal,
        state: &state,
        zone: &Local,
        today,
    };
    if args.apply {
        return settle.apply(io).await;
    }

    let normalizer = TimeNormalizer::for_settle(&config.vault_path)?;
    let model = ClaudeCliModel::new(config.plan_model.clone());
    let planning = Planning {
        model: &model,
        is_meeting: |aggregate: &DayProjectAggregate| normalizer.is_meeting_entry(aggregate),
    };
    let outcome = if args.replan {
        settle.replan(&planning, io).await?
    } else {
        settle.plan(args, &planning, io).await?
    };
    if outcome == Outcome::Ok {
        settle.period(io).await;
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// The three runs
// ---------------------------------------------------------------------------

/// What every run reads from. The zone is a field so a test can fix it; production passes
/// `Local`, the zone the Chrono entries are re-dated into.
struct Settle<'a, Tz: TimeZone> {
    config: &'a Config,
    chrono: &'a ChronoClient,
    portal: &'a TtApiClient,
    state: &'a State,
    zone: &'a Tz,
    today: NaiveDate,
}

/// What planning needs beyond the clients: the model, and the vault's meeting probe.
struct Planning<'a, M, P> {
    model: &'a M,
    is_meeting: P,
}

/// One batch of days to plan, with everything fetched for them.
struct DaysToPlan<'a> {
    days: &'a [NaiveDate],
    explicit: bool,
    entries: &'a [ChronoTimeEntry],
    portal: &'a [PortalDay],
    assigned: &'a HashMap<NaiveDate, Vec<Project>>,
    pins: &'a HashMap<NaiveDate, DayPins>,
}

/// The planned days, unnumbered, and the days that could not be or did not need to be.
struct Planned {
    days: Vec<DayPlan>,
    errors: Vec<DayError>,
    closed: Vec<NaiveDate>,
}

/// Why `--apply` stopped in the middle: the line it was writing and what went wrong.
struct Stop {
    date: NaiveDate,
    addr: u32,
    message: String,
}

impl<Tz: TimeZone> Settle<'_, Tz> {
    /// `tt-devpro settle`: build, show and store a plan. Writes nothing to DevPro.
    async fn plan<M, P>(
        &self,
        args: &SettleArgs,
        planning: &Planning<'_, M, P>,
        io: &mut dyn Console,
    ) -> Result<Outcome>
    where
        M: PlanModel,
        P: Fn(&DayProjectAggregate) -> bool,
    {
        // The session is checked before anything is fetched, so an expired cookie says so
        // first rather than after a minute of Chrono and portal reads.
        let user = self.portal.get_current_user().await?;

        let cutoff = last_settleable_day(self.today, args.include_today);
        let explicit = args.from.is_some() || args.to.is_some();
        let (start, end) = if explicit {
            let (range, note) = resolve_range(args.from, args.to, self.today, cutoff);
            if let Some(note) = note {
                io.err(&note);
            }
            if range.from > range.to {
                bail!("--from {} is after --to {}", range.from, range.to);
            }
            (range.from, range.to)
        } else {
            let start = self
                .today
                .checked_sub_days(Days::new(SCAN_DAYS))
                .ok_or_else(|| anyhow!("date underflow: {SCAN_DAYS} days before {}", self.today))?;
            (start, cutoff)
        };

        let entries = self.chrono_entries(start, end).await?;
        let portal = self.read_portal(recent_titles_start(start)?, end).await?;
        let (days, held_back) = if explicit {
            (
                start.iter_days().take_while(|day| *day <= end).collect(),
                Vec::new(),
            )
        } else {
            self.scan(&entries, &portal, start, args.include_today, io)?
        };
        let assigned = self.assigned(&user.unique_id, &days).await?;

        let planned = self
            .plan_days(
                planning,
                DaysToPlan {
                    days: &days,
                    explicit,
                    entries: &entries,
                    portal: &portal,
                    assigned: &assigned,
                    pins: &HashMap::new(),
                },
                io,
            )
            .await?;

        let plan = Plan {
            prefix: args.prefix.clone().unwrap_or_default(),
            cutoff: end,
            days: planned.days,
            errors: planned.errors,
            closed: planned.closed,
        };
        if plan.is_empty() && !held_back.is_empty() {
            // "All days are closed" on stdout while stderr reports today held back would be
            // two answers to one question; the held-back sentence is the true one.
            let text = nothing_to_settle_message(&held_back, self.today);
            self.state.save(&plan, &text)?;
            io.out(&text);
        } else {
            self.show(plan, io)?;
        }
        Ok(Outcome::Ok)
    }

    /// `tt-devpro settle --replan`: plan the edited days again around Yurii's edits.
    async fn replan<M, P>(
        &self,
        planning: &Planning<'_, M, P>,
        io: &mut dyn Console,
    ) -> Result<Outcome>
    where
        M: PlanModel,
        P: Fn(&DayProjectAggregate) -> bool,
    {
        let loaded = self.state.load()?;
        let rows = parse::parse(&loaded.md, &loaded.plan)?;
        let edits = parse::diff(&loaded.plan, &rows)?;
        let mut plan = loaded.plan;

        let mut rebuild: BTreeSet<NaiveDate> = edits.keys().copied().collect();
        rebuild.extend(plan.errors.iter().map(|error| error.date));
        if rebuild.is_empty() {
            // Shown again and stored again: a file touched without a change of meaning
            // (whitespace, a column realigned) gets its hash back, so --apply is not left
            // refusing a plan nobody changed.
            io.err("plan.md has no edits; the plan is unchanged.");
            self.show(plan, io)?;
            return Ok(Outcome::Ok);
        }

        let user = self.portal.get_current_user().await?;
        let days: Vec<NaiveDate> = rebuild.iter().copied().collect();
        let assigned = self.assigned(&user.unique_id, &days).await?;

        let mut pins: HashMap<NaiveDate, DayPins> = HashMap::new();
        for day in plan.days.iter().filter(|day| rebuild.contains(&day.date)) {
            let day_pins = self.day_pins(day, edits.get(&day.date), &assigned[&day.date], io)?;
            pins.insert(day.date, day_pins);
        }

        let (first, last) = (days[0], days[days.len() - 1]);
        let entries = self.chrono_entries(first, last).await?;
        let portal = self.read_portal(recent_titles_start(first)?, last).await?;
        let planned = self
            .plan_days(
                planning,
                DaysToPlan {
                    days: &days,
                    // Every day here was in the plan on purpose, so a day whose Chrono
                    // entries have gone says so rather than vanishing from the table.
                    explicit: true,
                    entries: &entries,
                    portal: &portal,
                    assigned: &assigned,
                    pins: &pins,
                },
                io,
            )
            .await?;

        // A day with edits that cannot be planned around them stops the replan before
        // anything is stored. Saving it as a day error would drop the edits — the error
        // keeps no pins — and the next --replan would build the day from scratch while the
        // message still names edits that are gone. plan.md stays as it is, to be corrected.
        let refused: Vec<String> = planned
            .errors
            .iter()
            .filter(|error| edits.contains_key(&error.date))
            .map(|error| format!("{}: {}", day_label(error.date), error.message))
            .collect();
        if !refused.is_empty() {
            bail!(
                "the edits in {} cannot be planned; correct them there and run --replan \
                 again:\n{}",
                self.state.md_path().display(),
                refused.join("\n")
            );
        }

        plan.days.retain(|day| !rebuild.contains(&day.date));
        plan.days.extend(planned.days);
        plan.errors = planned.errors;
        plan.closed.extend(planned.closed);
        plan.closed.sort();
        plan.closed.dedup();
        self.show(plan, io)?;
        Ok(Outcome::Ok)
    }

    /// The billing-period block under the plan just shown, read from the plan as stored.
    ///
    /// Its own failure is one ⚠️ line on stdout and the run stays successful: the plan is
    /// already shown and stored, and the block only reports.
    async fn period(&self, io: &mut dyn Console) {
        match self.period_block().await {
            Ok(lines) => {
                for line in lines {
                    io.out(&line);
                }
            }
            Err(error) => {
                io.out("");
                io.out(&format!("\u{26a0}\u{fe0f} Период не прочитан: {error:#}"));
            }
        }
    }

    async fn period_block(&self) -> Result<Vec<String>> {
        let plan = self.state.load()?.plan;
        let through = settle_period::through(plan.cutoff, self.today);
        let periods = self.portal.get_ptr_periods(&through.to_string()).await?;
        let period = settle_period::choose(&periods, through)?;
        let portal = self.read_portal(period.start, through).await?;
        Ok(settle_period::block(
            &period,
            through,
            &portal,
            &plan,
            &self.config.allocations,
        ))
    }

    /// `tt-devpro settle --apply`: write the plan last shown, then read DevPro back.
    async fn apply(&self, io: &mut dyn Console) -> Result<Outcome> {
        let loaded = self.state.load()?;
        if !loaded.md_unchanged() {
            bail!(
                "{} changed after it was shown. Run `tt-devpro settle --replan`, show the new \
                 plan, and apply that one.",
                self.state.md_path().display()
            );
        }
        let plan = loaded.plan;
        if plan.days.is_empty() {
            io.out("Nothing to apply: the plan has no planned days.");
            for error in &plan.errors {
                io.out(&error_line(error));
            }
            return Ok(Outcome::Ok);
        }
        check_ready(&plan)?;

        let user = self.portal.get_current_user().await?;
        let dates: Vec<NaiveDate> = plan.days.iter().map(|day| day.date).collect();
        let assigned = self.assigned(&user.unique_id, &dates).await?;
        self.check_projects(&plan, &assigned, io)?;

        let (first, last) = (dates[0], dates[dates.len() - 1]);
        let before = self.read_portal(first, last).await?;
        let before_by_day: HashMap<NaiveDate, &PortalDay> =
            before.iter().map(|day| (day.date, day)).collect();

        let mut changed: Vec<NaiveDate> = Vec::new();
        let mut written: Vec<(NaiveDate, usize)> = Vec::new();
        for day in &plan.days {
            let in_devpro = before_by_day.get(&day.date).copied();
            if recorded_ids(day) != portal_ids(in_devpro) {
                changed.push(day.date);
                continue;
            }
            match self.write_day(day, in_devpro, &plan.prefix, io).await {
                Ok(count) => written.push((day.date, count)),
                Err(stop) => return self.stopped(&stop, in_devpro, &plan, io).await,
            }
        }

        let after = self.read_portal(first, last).await?;
        let mut failed = !changed.is_empty();
        for (date, count) in &written {
            let in_devpro = after.iter().find(|day| day.date == *date);
            let total = in_devpro.map(portal_quarters).unwrap_or(Some(0));
            if total == Some(DAY_QUARTERS) {
                io.out(&format!(
                    "\u{2713} {} — 8.0 h in DevPro, {count} line(s) written",
                    day_label(*date)
                ));
            } else {
                failed = true;
                io.out(&format!(
                    "\u{2717} {} — DevPro holds {} h after the write, not 8.0",
                    day_label(*date),
                    in_devpro.map_or_else(|| "0.0".to_string(), |day| hours(day.logged_hours))
                ));
            }
        }
        for date in &changed {
            io.out(&format!(
                "\u{26A0}\u{FE0F} {} — not written: DevPro changed after the plan was shown. Run \
                 `tt-devpro settle` again.",
                day_label(*date)
            ));
        }
        for error in &plan.errors {
            io.out(&error_line(error));
        }

        // What is left for a later --replan: only the days that could not be planned.
        if plan.errors.is_empty() {
            self.state.clear()?;
        } else {
            let rest = Plan {
                days: Vec::new(),
                closed: Vec::new(),
                ..plan
            };
            let text = render(&rest);
            self.state.save(&rest, &text)?;
        }
        Ok(if failed { Outcome::Failed } else { Outcome::Ok })
    }

    // -- planning -----------------------------------------------------------

    async fn plan_days<M, P>(
        &self,
        planning: &Planning<'_, M, P>,
        batch: DaysToPlan<'_>,
        io: &mut dyn Console,
    ) -> Result<Planned>
    where
        M: PlanModel,
        P: Fn(&DayProjectAggregate) -> bool,
    {
        let context = plan_context::build(
            &Inputs {
                config: self.config,
                days: batch.days,
                explicit: batch.explicit,
                entries: batch.entries,
                portal: batch.portal,
                assigned: batch.assigned,
                pins: batch.pins,
            },
            &planning.is_meeting,
            self.zone,
        )?;
        warn_fallbacks(&context.fallbacks, io);
        let days = planner::plan(planning.model, &context, self.config.max_synthetic_hours).await?;
        Ok(Planned {
            days,
            errors: context.errors,
            closed: context.closed,
        })
    }

    /// The days the scan offers — Chrono days from `start` on, final, under 8 h in DevPro, not
    /// a weekend or a holiday — and the Chrono days it held back as not final.
    fn scan(
        &self,
        entries: &[ChronoTimeEntry],
        portal: &[PortalDay],
        start: NaiveDate,
        include_today: bool,
        io: &mut dyn Console,
    ) -> Result<(Vec<NaiveDate>, Vec<NaiveDate>)> {
        let mut days: Vec<NaiveDate> = Vec::new();
        for entry in entries {
            let day = aggregator::entry_local_date(&entry.start_time, self.zone)?;
            if day >= start && !days.contains(&day) {
                days.push(day);
            }
        }
        days.sort();
        let window = split_by_finality(&days, self.today, include_today);
        if !window.not_final.is_empty() {
            io.err(&format!(
                "\u{2139} Skipped (hours not final yet): {}",
                describe_not_final_days(&window.not_final, self.today)
            ));
        }
        let logged: HashMap<NaiveDate, f64> = portal
            .iter()
            .map(|day| (day.date, day.logged_hours))
            .collect();
        Ok((unfilled_days(&window.settleable, &logged), window.not_final))
    }

    /// Numbers, renders, stores and prints a plan.
    fn show(&self, mut plan: Plan, io: &mut dyn Console) -> Result<()> {
        plan.number();
        let text = render(&plan);
        self.state.save(&plan, &text)?;
        io.out(&text);
        Ok(())
    }

    // -- replanning ---------------------------------------------------------

    /// What a replan keeps fixed on one day: every line pinned before, plus this round's
    /// edits. Recorded lines are not carried; they come back from DevPro itself.
    fn day_pins(
        &self,
        day: &DayPlan,
        edits: Option<&DayEdits>,
        assigned: &[Project],
        io: &mut dyn Console,
    ) -> Result<DayPins> {
        let mut pins = DayPins::default();
        for line in day.lines.iter().filter(|line| line.pinned) {
            if line.kind == LineKind::Recorded {
                continue;
            }
            match &line.chrono {
                Some(key) => {
                    pins.chrono.insert(key.clone(), line.clone());
                }
                None => pins.synthetic.push(line.clone()),
            }
        }
        let Some(edits) = edits else {
            return Ok(pins);
        };

        for (line, row) in &edits.edited {
            let edited = self.edited_line(line, row, assigned, io)?;
            match &line.chrono {
                Some(key) => {
                    pins.chrono.insert(key.clone(), edited);
                }
                None => {
                    pins.synthetic.retain(|pinned| pinned.addr != line.addr);
                    pins.synthetic.push(edited);
                }
            }
        }
        for line in &edits.removed {
            match &line.chrono {
                Some(key) => {
                    pins.chrono.remove(key);
                    pins.removed.insert(key.clone());
                }
                None => pins.synthetic.retain(|pinned| pinned.addr != line.addr),
            }
        }
        for row in &edits.added {
            let (project_id, billability) = self.row_project(row, assigned, io)?;
            check_title(row, row.needs_detail)?;
            pins.synthetic.push(PlanLine {
                addr: 0,
                kind: LineKind::Manual,
                chrono: None,
                entry_count: 0,
                chrono_hours: None,
                title: row.title.clone(),
                needs_detail: row.needs_detail,
                devpro_project: row.devpro_project.clone(),
                project_id,
                billability,
                quarters: row.quarters,
                pinned: true,
                worklog_id: None,
                candidate_id: None,
            });
        }
        Ok(pins)
    }

    /// A shown line as the edited row now has it, pinned. A rewritten title drops ❓ even if
    /// the mark was left in place: the rewrite is the answer to it.
    fn edited_line(
        &self,
        line: &PlanLine,
        row: &Row,
        assigned: &[Project],
        io: &mut dyn Console,
    ) -> Result<PlanLine> {
        let billability = if row.billable { BILLABLE } else { NON_BILLABLE };
        let (project_id, billability) =
            if row.devpro_project == line.devpro_project && billability == line.billability {
                (line.project_id.clone(), line.billability.clone())
            } else {
                self.row_project(row, assigned, io)?
            };
        let needs_detail = row.needs_detail && row.title == line.title;
        check_title(row, needs_detail)?;
        Ok(PlanLine {
            title: row.title.clone(),
            needs_detail,
            devpro_project: row.devpro_project.clone(),
            project_id,
            billability,
            quarters: row.quarters,
            pinned: true,
            ..line.clone()
        })
    }

    /// The project id and billability a row names. The billability is read off 💵 in the
    /// project cell, and the pair must be one the config already uses: the same project is
    /// mapped billable from one Chrono project and non-billable from another, so the name
    /// alone cannot decide it, and a pair nobody configured is more likely a typo than a
    /// new arrangement.
    fn row_project(
        &self,
        row: &Row,
        assigned: &[Project],
        io: &mut dyn Console,
    ) -> Result<(String, String)> {
        let billability = if row.billable { BILLABLE } else { NON_BILLABLE };
        let project = row.devpro_project.as_str();
        let config = self.config;
        let known = config
            .mappings
            .iter()
            .any(|m| m.devpro_project == project && m.billability == billability)
            || config
                .overrides
                .iter()
                .any(|o| o.devpro_project == project && o.billability == billability)
            || config
                .fillers
                .iter()
                .any(|f| f.devpro_project == project && f.billability == billability);
        if !known {
            bail!(
                "plan.md line {}: {project} as {billability} is in none of mappings, overrides or \
                 fillers in ~/.config/tt-devpro/config.yaml",
                row.line_no
            );
        }
        let resolution = resolve_project_ids(&[project.to_string()], assigned, &config.project_ids)
            .map_err(|error| anyhow!("plan.md line {}: {error:#}", row.line_no))?;
        warn_fallbacks(&resolution.fallbacks, io);
        let id = resolution
            .ids_by_name
            .get(project)
            .cloned()
            .ok_or_else(|| anyhow!("plan.md line {}: {project} did not resolve", row.line_no))?;
        Ok((id, billability.to_string()))
    }

    // -- applying -----------------------------------------------------------

    /// Every line still resolves to the project id it was planned with. An assignment that
    /// changed between the plan and the write would otherwise post to a project the plan
    /// did not name.
    fn check_projects(
        &self,
        plan: &Plan,
        assigned: &HashMap<NaiveDate, Vec<Project>>,
        io: &mut dyn Console,
    ) -> Result<()> {
        for day in &plan.days {
            for line in day
                .lines
                .iter()
                .filter(|line| line.kind != LineKind::Recorded)
            {
                let resolution = resolve_project_ids(
                    std::slice::from_ref(&line.devpro_project),
                    &assigned[&day.date],
                    &self.config.project_ids,
                )
                .map_err(|error| {
                    anyhow!("{}{} on {}: {error:#}", plan.prefix, line.addr, day.date)
                })?;
                warn_fallbacks(&resolution.fallbacks, io);
                let id = &resolution.ids_by_name[&line.devpro_project];
                if *id != line.project_id {
                    bail!(
                        "{}{}: {} resolves to {id} on {} now, but the plan has {}. Run \
                         `tt-devpro settle` again.",
                        plan.prefix,
                        line.addr,
                        line.devpro_project,
                        day.date,
                        line.project_id
                    );
                }
            }
        }
        Ok(())
    }

    /// Creates a day's new lines, one at a time. Returns how many were written, or where it
    /// stopped. A timeout is not retried: the request may have landed, so DevPro is read
    /// and the line counts as written only if it is there.
    async fn write_day(
        &self,
        day: &DayPlan,
        before: Option<&PortalDay>,
        prefix: &str,
        io: &mut dyn Console,
    ) -> Result<usize, Stop> {
        let before_ids: HashSet<&str> = before
            .map(|day| day.worklogs.iter().map(|w| w.unique_id.as_str()).collect())
            .unwrap_or_default();
        let mut written: Vec<&PlanLine> = Vec::new();
        for line in day
            .lines
            .iter()
            .filter(|line| line.kind != LineKind::Recorded)
        {
            let stop = |message: String| Stop {
                date: day.date,
                addr: line.addr,
                message,
            };
            match self
                .portal
                .create_worklog(&create_request(day.date, line))
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return Err(stop(
                        "DevPro did not confirm the write (no 200)".to_string(),
                    ));
                }
                Err(error) if is_timeout(&error) => {
                    let landed = self
                        .landed(day.date, line, &before_ids, &written)
                        .await
                        .map_err(|error| stop(format!("{error:#}")))?;
                    if !landed {
                        return Err(stop(
                            "the write timed out and the line is not in DevPro; it is not \
                             sent again, because it may still land"
                                .to_string(),
                        ));
                    }
                    io.err(&format!(
                        "\u{2139} {prefix}{}: the write timed out, but the line is in DevPro",
                        line.addr
                    ));
                }
                Err(error) => return Err(stop(failure_text(&error))),
            }
            written.push(line);
        }
        Ok(written.len())
    }

    /// Whether `line` reached DevPro after a timeout: a worklog that was not there before
    /// the run, with the same project, title and hours, beyond the identical lines this run
    /// already wrote to the day.
    async fn landed(
        &self,
        date: NaiveDate,
        line: &PlanLine,
        before_ids: &HashSet<&str>,
        written: &[&PlanLine],
    ) -> Result<bool> {
        let month = date.with_day(1).expect("every month has a first day");
        let view = self.portal.get_normal_view(&month.to_string()).await?;
        let days = plan_context::portal_days(&[view])?;
        let Some(day) = days.iter().find(|day| day.date == date) else {
            return Ok(false);
        };
        let same = |w: &WorklogDetail| {
            w.project_unique_id == line.project_id
                && w.task_title == line.title
                && hours_to_quarters(w.logged_hours) == Some(line.quarters)
        };
        let new_matches = day
            .worklogs
            .iter()
            .filter(|w| !before_ids.contains(w.unique_id.as_str()) && same(w))
            .count();
        let earlier = written
            .iter()
            .filter(|l| {
                l.project_id == line.project_id
                    && l.title == line.title
                    && l.quarters == line.quarters
            })
            .count();
        Ok(new_matches > earlier)
    }

    /// The write stopped: take the stopped day back to what it held before the run, say
    /// where it stopped, show what DevPro holds now, and drop the plan.
    ///
    /// A day is written whole or not at all. The next `settle` cannot tell which Chrono line
    /// a written worklog came from — the title was rewritten on the way — so a half-written
    /// day would come back with its written lines as recorded and every Chrono line proposed
    /// again beside them: the same work twice, still adding up to 8.0 h. Removing only the
    /// worklogs this run created on that day leaves DevPro in a state the next plan reads
    /// right; days written whole before it stay.
    async fn stopped(
        &self,
        stop: &Stop,
        before: Option<&PortalDay>,
        plan: &Plan,
        io: &mut dyn Console,
    ) -> Result<Outcome> {
        io.err(&format!(
            "\u{2717} {} {}{}: {}",
            day_label(stop.date),
            plan.prefix,
            stop.addr,
            stop.message
        ));
        match self.roll_back(stop.date, before).await {
            Ok(0) => {}
            Ok(removed) => io.err(&format!(
                "\u{21A9} {} — the {removed} line(s) this run had written to the day are removed \
                 again: a day goes to DevPro whole or not at all",
                day_label(stop.date)
            )),
            Err(left) => io.err(&format!(
                "\u{26A0}\u{FE0F} {} — {left}. Until they are gone, the next settle plans this \
                 day's work a second time beside them.",
                day_label(stop.date)
            )),
        }
        io.err("Writing stopped. DevPro now holds:");
        let first = plan.days[0].date;
        let last = plan.days[plan.days.len() - 1].date;
        match self.read_portal(first, last).await {
            Ok(days) => {
                for planned in &plan.days {
                    let in_devpro = days.iter().find(|day| day.date == planned.date);
                    io.err(&format!("  {}", devpro_day_line(planned.date, in_devpro)));
                }
            }
            Err(error) => io.err(&format!("  DevPro could not be read: {error:#}")),
        }
        self.state.clear()?;
        io.err("The plan is dropped. Run `tt-devpro settle` again: it shows what was written as lines already in DevPro.");
        Ok(Outcome::Failed)
    }

    /// Deletes the worklogs on `date` that were not there before the run: the ones this run
    /// created. Returns how many, or what is left and the command that removes each.
    async fn roll_back(
        &self,
        date: NaiveDate,
        before: Option<&PortalDay>,
    ) -> Result<usize, String> {
        let before_ids: HashSet<&str> = before
            .map(|day| day.worklogs.iter().map(|w| w.unique_id.as_str()).collect())
            .unwrap_or_default();
        let now = self
            .read_portal(date, date)
            .await
            .map_err(|error| format!("DevPro could not be read to undo the day ({error:#})"))?;
        let created: Vec<String> = now
            .iter()
            .filter(|day| day.date == date)
            .flat_map(|day| &day.worklogs)
            .filter(|w| !before_ids.contains(w.unique_id.as_str()))
            .map(|w| w.unique_id.clone())
            .collect();
        for (done, id) in created.iter().enumerate() {
            let failure = match self.portal.delete_worklog(id).await {
                Ok(true) => continue,
                Ok(false) => "DevPro did not confirm the delete (no 200)".to_string(),
                Err(error) => failure_text(&error),
            };
            let left: Vec<String> = created[done..]
                .iter()
                .map(|id| format!("`tt-devpro api delete-worklog {id}`"))
                .collect();
            return Err(format!(
                "undoing the day stopped ({failure}); this run's lines still there: {}",
                left.join(", ")
            ));
        }
        Ok(created.len())
    }

    // -- fetching -----------------------------------------------------------

    /// Chrono from the working week before `first`, for the history candidates, to the day
    /// after `last`: entries are re-dated to their local day, so the extra UTC day can never
    /// add a local date past `last`.
    async fn chrono_entries(
        &self,
        first: NaiveDate,
        last: NaiveDate,
    ) -> Result<Vec<ChronoTimeEntry>> {
        let start = working_days_before(first, HISTORY_WORKING_DAYS)
            .last()
            .copied()
            .unwrap_or(first);
        let end = last
            .succ_opt()
            .ok_or_else(|| anyhow!("date overflow: the day after {last}"))?;
        self.chrono.get_time_entries(start, end).await
    }

    /// `normalView` for every month `[start, end]` touches. The endpoint keys on the month,
    /// so one request per month is the whole answer.
    async fn read_portal(&self, start: NaiveDate, end: NaiveDate) -> Result<Vec<PortalDay>> {
        let mut views = Vec::new();
        for month in months_in_range(start, end) {
            views.push(self.portal.get_normal_view(&month.to_string()).await?);
        }
        plan_context::portal_days(&views)
    }

    /// The projects assigned on each day. Assignments are per date, so each day asks.
    async fn assigned(
        &self,
        contact_id: &str,
        days: &[NaiveDate],
    ) -> Result<HashMap<NaiveDate, Vec<Project>>> {
        let mut assigned = HashMap::new();
        for day in days {
            let response = self
                .portal
                .get_assigned_projects(contact_id, &day.to_string())
                .await?;
            assigned.insert(*day, response.projects);
        }
        Ok(assigned)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn recent_titles_start(first: NaiveDate) -> Result<NaiveDate> {
    first
        .checked_sub_days(Days::new(RECENT_TITLE_DAYS))
        .ok_or_else(|| anyhow!("date underflow: {RECENT_TITLE_DAYS} days before {first}"))
}

/// Printed once per project for the run: a configured id stood in for one the portal did
/// not list, and a stale one would post to the wrong project with nothing else to show it.
fn warn_fallbacks(fallbacks: &[FallbackId], io: &mut dyn Console) {
    for fallback in fallbacks {
        io.err(&format!(
            "\u{26A0} '{}' is not in your assigned projects \u{2014} using id {} from project_ids in ~/.config/tt-devpro/config.yaml. Check it still points at the right project.",
            fallback.name, fallback.id
        ));
    }
}

/// A title that goes to DevPro is plain ASCII English within the length the model is held
/// to. One still marked ❓ may be anything: `--apply` refuses it until it is rewritten.
fn check_title(row: &Row, needs_detail: bool) -> Result<()> {
    if needs_detail {
        return Ok(());
    }
    if !is_plain_ascii(&row.title) {
        bail!(
            "plan.md line {}: \"{}\" is not plain ASCII; DevPro gets English only",
            row.line_no,
            row.title
        );
    }
    if row.title.chars().count() > MAX_TITLE_CHARS {
        bail!(
            "plan.md line {}: the title is longer than {MAX_TITLE_CHARS} characters",
            row.line_no
        );
    }
    Ok(())
}

/// What `--apply` checks before the first write, so a plan that cannot be written whole
/// writes nothing.
fn check_ready(plan: &Plan) -> Result<()> {
    let vague: Vec<String> = plan
        .days
        .iter()
        .flat_map(|day| &day.lines)
        .filter(|line| line.needs_detail)
        .map(|line| format!("{}{}", plan.prefix, line.addr))
        .collect();
    if !vague.is_empty() {
        bail!(
            "\u{2753} marks a title that cannot go to DevPro as it is: {}. Rewrite them in \
             plan.md, then run --replan.",
            vague.join(", ")
        );
    }
    for day in &plan.days {
        if day.total_quarters() != DAY_QUARTERS {
            bail!(
                "{} comes to {} h, not 8.0",
                day_label(day.date),
                hours(quarters_to_hours(day.total_quarters()))
            );
        }
        if day.date > plan.cutoff {
            bail!(
                "{} is past the last day this plan may write ({})",
                day.date,
                plan.cutoff
            );
        }
    }
    Ok(())
}

/// The worklog ids the plan showed as already in DevPro for a day.
pub(crate) fn recorded_ids(day: &DayPlan) -> BTreeSet<&str> {
    day.lines
        .iter()
        .filter(|line| line.kind == LineKind::Recorded)
        .filter_map(|line| line.worklog_id.as_deref())
        .collect()
}

pub(crate) fn portal_ids(day: Option<&PortalDay>) -> BTreeSet<&str> {
    day.map(|day| day.worklogs.iter().map(|w| w.unique_id.as_str()).collect())
        .unwrap_or_default()
}

/// A day's worklogs in quarters, or `None` when one of them is off the grid.
fn portal_quarters(day: &PortalDay) -> Option<Quarters> {
    day.worklogs
        .iter()
        .map(|w| hours_to_quarters(w.logged_hours))
        .sum()
}

fn devpro_day_line(date: NaiveDate, day: Option<&PortalDay>) -> String {
    let Some(day) = day.filter(|day| !day.worklogs.is_empty()) else {
        return format!("{} — nothing", day_label(date));
    };
    let lines: Vec<String> = day
        .worklogs
        .iter()
        .map(|w| {
            format!(
                "{} ({} h, {})",
                w.task_title,
                hours(w.logged_hours),
                w.project_short_name
            )
        })
        .collect();
    format!(
        "{} — {} h: {}",
        day_label(date),
        hours(day.logged_hours),
        lines.join("; ")
    )
}

fn error_line(error: &DayError) -> String {
    format!(
        "\u{26A0}\u{FE0F} {} — not planned, kept for --replan: {}",
        day_label(error.date),
        error.message
    )
}

/// The create body. `expenseType` is the literal `"None"` and every other optional field is
/// an explicit null, which is what the portal has always received from this tool.
fn create_request(date: NaiveDate, line: &PlanLine) -> CreateWorklogRequest {
    CreateWorklogRequest {
        worklog_date: date.to_string(),
        project_unique_id: line.project_id.clone(),
        task_title: line.title.clone(),
        billability: line.billability.clone(),
        duration: quarters_to_hours(line.quarters),
        description: None,
        overtime: None,
        expense_type: Some("None".to_string()),
        pif: None,
        google_calendar_event_id: None,
    }
}

fn is_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)
    })
}

fn failure_text(error: &anyhow::Error) -> String {
    match error.downcast_ref::<ApiError>() {
        Some(api) => format!("API Error: {api}"),
        None => format!("{error:#}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

pub mod replay;

#[cfg(test)]
mod tests;
