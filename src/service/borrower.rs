//! Borrowing last week's tasks onto a meeting-heavy day that still falls short of 8h.
//!
//! Ports `service/BorrowerService.kt`. Carries the borrowing half of **C6** and the
//! secondary site of **C12**.
//!
//! Four things in this file look like defects and are the incumbent's behaviour, so
//! they are ported verbatim and named here rather than left for the next reader to
//! "fix":
//!
//! - **History is anchored to the earliest day of the whole run, not to each
//!   shortfall day** (`BorrowerService.kt:76-78`). `byDate.keys.minOrNull()` is taken
//!   once, so a shortfall day two weeks after the earliest day still borrows from the
//!   week before that *earliest* day. Plan, `## Kotlin→Rust semantics`.
//! - **The frequency map is last-wins** (`BorrowerService.kt:122-123`). A description met under two
//!   aggregates accumulates both sets of hours and keeps the **last** aggregate's
//!   project, billability and source date. `entry().or_insert()` keeps the first and
//!   is therefore the wrong idiom here.
//! - **A failed history fetch is swallowed in silence** (`BorrowerService.kt:80-85`). No
//!   message, no error, no borrowing. D3's non-zero exit path must not capture it.
//! - **The hours handed out may exceed the shortfall** (`BorrowerService.kt:155-156`).
//!   `maxAllowed` is capped at `hoursToFill` and *then* rounded, so a `hoursToFill` of
//!   `0.4` yields `0.5`.
//!
//! C28 governs two structures here, and in both of them order decides *content* —
//! which task is borrowed, and therefore how many hours land on which project in the
//! system of record — not layout:
//!
//! - `taskFrequency` (`BorrowerService.kt:118`) is a `mutableMapOf`, i.e. a
//!   `LinkedHashMap` in insertion order. It is a `Vec` walked in order below. A `HashMap`
//!   would randomize the pre-sort order per process.
//! - `sortedByDescending` (`BorrowerService.kt:128`) is a stable TimSort, so tasks tying
//!   on accumulated hours keep that insertion order. `sort_by` with the comparator
//!   reversed, never `sort_unstable_by`; `total_cmp`, never `partial_cmp`, because Kotlin
//!   compares through `Double.compareTo`'s total order.
//!
//! Two departures from the Kotlin signature, both sanctioned by the plan's module map
//! and neither a behaviour change:
//!
//! - the live `ChronoClient` becomes a [`HistorySource`] trait, so the "a failed fetch
//!   means no borrowing" rule is testable without a mock HTTP layer;
//! - `TimeNormalizer` arrives as a parameter rather than being constructed here,
//!   because it reads `~/knowledge-base` off disk and meeting detection on the
//!   *history* window would otherwise depend on the machine running the tests.

use std::collections::HashSet;
use std::future::Future;

use anyhow::{Result, anyhow};
use chrono::{Days, NaiveDate};

use crate::commands::settle_render::clean_task_title;
use crate::config::Config;
use crate::model::{ChronoTimeEntry, DayProjectAggregate, FillerEntry, NormalizedAggregate};
use crate::service::aggregator;
use crate::service::normalizer::{HOUR_INCREMENT, TimeNormalizer, java_min, round_to_quarter};

/// `BorrowerService.kt:16`.
const TARGET_HOURS: f64 = 8.0;

/// `BorrowerService.kt:18`.
const LOOKBACK_DAYS: u64 = 7;

/// `BorrowerService.kt:20-27`. Stays in this module: its only consumer is
/// `commands::settle`, which is built after it.
#[derive(Debug, Clone, PartialEq)]
pub struct BorrowedEntry {
    pub date: NaiveDate,
    /// The date we borrowed from.
    pub source_date: NaiveDate,
    pub devpro_project_name: String,
    pub task_title: String,
    pub billability: String,
    pub hours: f64,
}

/// The one thing [`borrow_for_meeting_only_days`] needs off the network, behind a
/// trait so that C6's "a failed history fetch means no borrowing, never an error"
/// has an oracle. `api::chrono::ChronoClient` implements it in step 3.
///
/// Declared as `-> impl Future` rather than `async fn` so the `Send` bound can be
/// stated: `async fn` in a public trait warns for exactly that reason.
pub trait HistorySource {
    /// `ChronoClient.getTimeEntries`, called at `BorrowerService.kt:81`.
    fn get_time_entries(
        &self,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> impl Future<Output = Result<Vec<ChronoTimeEntry>>> + Send;
}

/// `BorrowerService.borrowForMeetingOnlyDays` (`BorrowerService.kt:34-104`).
///
/// `max_synthetic_hours` is the per-day ceiling on borrowed **plus** filler hours;
/// what is left of it after the day's fillers is the borrowing budget.
///
/// Returns `Err` only where Kotlin throws: an unparseable Chrono timestamp or an
/// unmapped Chrono project out of [`aggregator::aggregate`] at `BorrowerService.kt:92`, which sits
/// outside the `try` at `BorrowerService.kt:80-85`. The fetch itself never produces an `Err`.
pub async fn borrow_for_meeting_only_days<H: HistorySource>(
    normalized: &[NormalizedAggregate],
    fillers: &[FillerEntry],
    history: &H,
    config: &Config,
    max_synthetic_hours: f64,
    normalizer: &TimeNormalizer,
) -> Result<Vec<BorrowedEntry>> {
    // `BorrowerService.kt:42` — `groupBy { it.original.date }`. C28: a `LinkedHashMap`,
    // so keys are in first-encounter order and each group holds its entries in input
    // order. A `Vec` of pairs walked in order, per the plan's list of faithful options.
    let mut by_date: Vec<(NaiveDate, Vec<&NormalizedAggregate>)> = Vec::new();
    for entry in normalized {
        match by_date
            .iter_mut()
            .find(|(date, _)| *date == entry.original.date)
        {
            Some((_, bucket)) => bucket.push(entry),
            None => by_date.push((entry.original.date, vec![entry])),
        }
    }

    // `BorrowerService.kt:43-69` — `mapNotNull { … }.toMap()`, another `LinkedHashMap`;
    // the dates are already unique so only the order carries.
    let mut days_with_shortfall: Vec<(NaiveDate, f64)> = Vec::new();
    for (date, day_entries) in &by_date {
        let entry_hours: f64 = day_entries.iter().map(|e| e.normalized_hours).sum();
        let filler_hours: f64 = fillers
            .iter()
            .filter(|f| f.date == *date)
            .map(|f| f.hours)
            .sum();
        let total_hours = entry_hours + filler_hours;
        let shortfall = TARGET_HOURS - total_hours;

        // `BorrowerService.kt:53-55` — what is left of the synthetic budget once the
        // fillers have taken their share, and the shortfall capped to it.
        let remaining_synthetic_budget = max_synthetic_hours - filler_hours;
        let capped_shortfall = java_min(shortfall, remaining_synthetic_budget);

        // `BorrowerService.kt:59-61` — `>=`, not `>`. A day whose meeting hours exactly
        // equal its work hours is meeting-heavy.
        let meeting_hours: f64 = day_entries
            .iter()
            .filter(|e| e.is_meeting)
            .map(|e| e.normalized_hours)
            .sum();
        let work_hours: f64 = day_entries
            .iter()
            .filter(|e| !e.is_meeting)
            .map(|e| e.normalized_hours)
            .sum();
        let is_meeting_heavy = meeting_hours >= work_hours;

        // `BorrowerService.kt:63` — strictly greater, so a shortfall of exactly one
        // increment does not borrow.
        if capped_shortfall > HOUR_INCREMENT && is_meeting_heavy {
            days_with_shortfall.push((*date, capped_shortfall));
        }
    }

    // `BorrowerService.kt:71-73`. The history fetch does not happen at all when nothing is short.
    if days_with_shortfall.is_empty() {
        return Ok(Vec::new());
    }

    // `BorrowerService.kt:76` — the minimum over **every** day in the run, not over the
    // shortfall days. Unreachable as `None`: an empty `by_date` cannot produce a
    // shortfall day.
    let Some(earliest_date) = by_date.iter().map(|(date, _)| *date).min() else {
        return Ok(Vec::new());
    };

    // `BorrowerService.kt:77-78`. `LocalDate.minusDays` throws past `LocalDate.MIN`;
    // `checked_sub_days` returns `None` at `NaiveDate::MIN`, and both are errors out of
    // this function.
    let history_start_date = earliest_date
        .checked_sub_days(Days::new(LOOKBACK_DAYS))
        .ok_or_else(|| anyhow!("Date underflow: {earliest_date} minus {LOOKBACK_DAYS} days"))?;
    let history_end_date = earliest_date
        .pred_opt()
        .ok_or_else(|| anyhow!("Date underflow: {earliest_date} minus 1 day"))?;

    // `BorrowerService.kt:81` — the fetch runs to `historyEndDate.plusDays(1)`, i.e. to
    // `earliestDate` itself. That is C2's `+1 day` UTC-axis padding, so a late local
    // evening filed under the next UTC day is still fetched; the aggregate filter below
    // then puts it back on its own local day and drops it if it falls outside the window.
    // The two bounds differ on purpose — do not make them agree.
    let history_fetch_end = history_end_date
        .succ_opt()
        .ok_or_else(|| anyhow!("Date overflow: {history_end_date} plus 1 day"))?;

    // `BorrowerService.kt:80-85` — every exception swallowed, no message, no borrowing.
    let Ok(history_entries) = history
        .get_time_entries(history_start_date, history_fetch_end)
        .await
    else {
        return Ok(Vec::new());
    };

    // `BorrowerService.kt:87-89`.
    if history_entries.is_empty() {
        return Ok(Vec::new());
    }

    // `BorrowerService.kt:92` — note the bounds: `[earliest-7, earliest-1]`, one day
    // tighter than the fetch above.
    let historical_aggregates = aggregator::aggregate(
        &history_entries,
        config,
        Some(history_start_date),
        Some(history_end_date),
    )?;

    // `BorrowerService.kt:95-98` — normalization is run only for its `isMeeting` verdict;
    // what survives is `it.original`, the *unnormalized* aggregate, so the hours borrowed
    // below are the raw Chrono hours and not the scaled ones. Meetings are never
    // borrowable.
    let normalized_history = normalizer.normalize(&historical_aggregates);
    let non_meeting_aggregates: Vec<DayProjectAggregate> = normalized_history
        .into_iter()
        .filter(|e| !e.is_meeting)
        .map(|e| e.original)
        .collect();

    // `BorrowerService.kt:101-103` — days in the insertion order established above.
    let mut result: Vec<BorrowedEntry> = Vec::new();
    for (date, shortfall) in &days_with_shortfall {
        result.extend(borrow_for_day(*date, *shortfall, &non_meeting_aggregates));
    }
    Ok(result)
}

/// `BorrowerService.borrowForDay` (`BorrowerService.kt:106-172`).
///
/// Kotlin's fourth parameter is `config: Config`, which the body never reads; it is
/// dropped here rather than carried as `_config`.
fn borrow_for_day(
    date: NaiveDate,
    shortfall: f64,
    historical_aggregates: &[DayProjectAggregate],
) -> Vec<BorrowedEntry> {
    // `BorrowerService.kt:112`.
    if historical_aggregates.is_empty() {
        return Vec::new();
    }

    // `BorrowerService.kt:118-125`. C28's first structure: insertion-ordered, because the
    // stable sort below turns this order into the tie-break that decides which tasks get
    // borrowed.
    //
    // The write at `BorrowerService.kt:122-123` is last-wins — read the pair, keep only
    // its hours, put back `(hours + share, agg)` with the *current* aggregate. So a
    // description met under two aggregates accumulates both shares while its project,
    // billability and source date come from the later one.
    let mut task_frequency: Vec<(&str, (f64, &DayProjectAggregate))> = Vec::new();
    for agg in historical_aggregates {
        for desc in &agg.descriptions {
            let key = desc.as_str();
            // `BorrowerService.kt:123` — `coerceAtLeast(1)`. Dead as written, since an
            // aggregate with no descriptions never enters this loop, but ported rather
            // than reasoned away.
            let share = agg.total_hours / agg.descriptions.len().max(1) as f64;
            match task_frequency.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = (slot.1.0 + share, agg),
                None => task_frequency.push((key, (share, agg))),
            }
        }
    }

    // `BorrowerService.kt:127-129` — `sortedByDescending { it.value.first }`, a stable TimSort.
    task_frequency.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    let sorted_tasks: Vec<(&str, &DayProjectAggregate)> = task_frequency
        .iter()
        .map(|(key, value)| (*key, value.1))
        .collect();

    // `BorrowerService.kt:131`.
    if sorted_tasks.is_empty() {
        return Vec::new();
    }

    let mut result: Vec<BorrowedEntry> = Vec::new();
    let mut hours_to_fill = shortfall;
    let mut task_index = 0usize;
    // `BorrowerService.kt:136` — a `LinkedHashSet` in Kotlin, but it is only ever probed
    // and added to, never iterated, so nothing observable depends on its order.
    let mut used_task_titles: HashSet<String> = HashSet::new();

    // `BorrowerService.kt:139` — `>=`, so a remaining fill of exactly one increment still
    // runs a pass.
    while hours_to_fill >= HOUR_INCREMENT && task_index < sorted_tasks.len() {
        let (task_description, source_aggregate) = sorted_tasks[task_index];
        // `BorrowerService.kt:141` — advanced before the `continue` below, which is what stops the
        // dedupe skip from spinning.
        task_index += 1;

        // `BorrowerService.kt:144-148` — C12's secondary site, textually identical to
        // `SettleCommand.kt:486,490,494-496`. One function, called from both.
        let clean_title = clean_task_title(task_description, &source_aggregate.chrono_project);

        // `BorrowerService.kt:151-152` — the dedupe key is the *cleaned* title, and it is
        // marked used before the `hours > 0` test below. A task whose share rounds to
        // zero therefore still burns its title for the rest of the day.
        if used_task_titles.contains(&clean_title) {
            continue;
        }
        used_task_titles.insert(clean_title.clone());

        // `BorrowerService.kt:155-156` — the cap is applied *before* the rounding, so the
        // result can exceed `hoursToFill`: `0.4` becomes `0.5`. Incumbent behaviour.
        let max_allowed = java_min(
            source_aggregate.total_hours / source_aggregate.descriptions.len().max(1) as f64,
            hours_to_fill,
        );
        let hours = round_to_quarter(max_allowed);

        // `BorrowerService.kt:158-168`.
        if hours > 0.0 {
            result.push(BorrowedEntry {
                date,
                source_date: source_aggregate.date,
                devpro_project_name: source_aggregate.devpro_project_name.clone(),
                task_title: clean_title,
                billability: source_aggregate.billability.clone(),
                hours,
            });
            hours_to_fill -= hours;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::config::ProjectMapping;

    use super::*;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    const CHRONO_PROJECT: &str = "Practices - DevPro - Work";
    const DEVPRO_PROJECT: &str = "Delivery Practices";

    fn d(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    fn agg(
        date: NaiveDate,
        chrono_project: &str,
        total_hours: f64,
        descriptions: &[&str],
        devpro_project: &str,
        billability: &str,
    ) -> DayProjectAggregate {
        DayProjectAggregate {
            date,
            chrono_project: chrono_project.to_string(),
            total_hours,
            descriptions: descriptions.iter().map(|s| (*s).to_string()).collect(),
            devpro_project_name: devpro_project.to_string(),
            billability: billability.to_string(),
            max_hours: None,
        }
    }

    /// A one-description aggregate on the default project.
    fn task(date: NaiveDate, description: &str, total_hours: f64) -> DayProjectAggregate {
        agg(
            date,
            CHRONO_PROJECT,
            total_hours,
            &[description],
            DEVPRO_PROJECT,
            "NonBillable",
        )
    }

    fn norm(aggregate: DayProjectAggregate, hours: f64, is_meeting: bool) -> NormalizedAggregate {
        NormalizedAggregate {
            original: aggregate,
            normalized_hours: hours,
            is_meeting,
        }
    }

    fn meeting(date: NaiveDate, description: &str, hours: f64) -> NormalizedAggregate {
        norm(task(date, description, hours), hours, true)
    }

    fn work(date: NaiveDate, description: &str, hours: f64) -> NormalizedAggregate {
        norm(task(date, description, hours), hours, false)
    }

    fn filler(date: NaiveDate, hours: f64) -> FillerEntry {
        FillerEntry {
            date,
            devpro_project_name: DEVPRO_PROJECT.to_string(),
            task_title: "Internal work".to_string(),
            billability: "NonBillable".to_string(),
            hours,
        }
    }

    fn config() -> Config {
        Config {
            chrono_api: "http://localhost:9247".to_string(),
            mappings: vec![ProjectMapping {
                chrono_project: CHRONO_PROJECT.to_string(),
                devpro_project: DEVPRO_PROJECT.to_string(),
                billability: "NonBillable".to_string(),
            }],
            fillers: Vec::new(),
            overrides: Vec::new(),
            project_ids: HashMap::new(),
            max_synthetic_hours: 4.0,
        }
    }

    /// A `TimeNormalizer` pointed at an empty directory: no `Calendar` folder exists,
    /// so C26's filename probe never fires and only an `Operations -` project counts
    /// as a meeting. Without this the history's meeting verdicts would depend on the
    /// contents of the developer's own vault.
    fn offline_normalizer() -> (tempfile::TempDir, TimeNormalizer) {
        let dir = tempfile::tempdir().expect("temp dir");
        let normalizer = TimeNormalizer::with_knowledge_base(dir.path());
        (dir, normalizer)
    }

    /// One Chrono entry, midday UTC so its local date equals its UTC date under every
    /// offset in `(-12, +12)` — the aggregator re-dates through the system zone (C2)
    /// and these tests must not depend on which zone that is.
    fn chrono_entry(id: i64, date: NaiveDate, description: &str, seconds: i64) -> ChronoTimeEntry {
        ChronoTimeEntry {
            id,
            description: Some(description.to_string()),
            start_time: format!("{date}T12:00:00Z"),
            end_time: None,
            duration: Some(seconds),
            project: Some(crate::model::ChronoProject {
                id: 1,
                name: CHRONO_PROJECT.to_string(),
                color: "#fff".to_string(),
                aspect: None,
            }),
            aspect: None,
        }
    }

    /// Records the window it was asked for, so the two different bounds at
    /// `BorrowerService.kt:78` and `BorrowerService.kt:81` can be asserted separately.
    struct FixtureHistory {
        entries: Vec<ChronoTimeEntry>,
        calls: Mutex<Vec<(NaiveDate, NaiveDate)>>,
    }

    impl FixtureHistory {
        fn new(entries: Vec<ChronoTimeEntry>) -> Self {
            Self {
                entries,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(NaiveDate, NaiveDate)> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl HistorySource for FixtureHistory {
        async fn get_time_entries(
            &self,
            start_date: NaiveDate,
            end_date: NaiveDate,
        ) -> Result<Vec<ChronoTimeEntry>> {
            self.calls
                .lock()
                .expect("lock")
                .push((start_date, end_date));
            Ok(self.entries.clone())
        }
    }

    /// The stub C6's "a failed history fetch means no borrowing, never an error" rule
    /// has no oracle without.
    struct FailingHistory {
        calls: Mutex<usize>,
    }

    impl FailingHistory {
        fn new() -> Self {
            Self {
                calls: Mutex::new(0),
            }
        }
    }

    impl HistorySource for FailingHistory {
        async fn get_time_entries(
            &self,
            _start_date: NaiveDate,
            _end_date: NaiveDate,
        ) -> Result<Vec<ChronoTimeEntry>> {
            *self.calls.lock().expect("lock") += 1;
            Err(anyhow!("connection refused"))
        }
    }

    // -----------------------------------------------------------------------
    // The day gate — `BorrowerService.kt:43-69`
    // -----------------------------------------------------------------------

    /// C6, `BorrowerService.kt:63`. The test is `cappedShortfall > HOUR_INCREMENT`,
    /// strictly. A day at 7.75h is short by exactly one increment and must not borrow.
    #[tokio::test]
    async fn a_shortfall_of_exactly_one_increment_does_not_borrow() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 7.75)];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed, Vec::new());
        assert_eq!(
            history.calls(),
            Vec::new(),
            "no shortfall day means the history is never fetched"
        );
    }

    /// C6, `BorrowerService.kt:63`. The other side of the same `>`: 7.74h is short by
    /// 0.26h, which clears the gate. Together with the test above this pins the
    /// boundary rather than sampling around it.
    #[tokio::test]
    async fn a_shortfall_just_over_one_increment_borrows() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 7.74)];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0].task_title, "Past work");
        // `min(0.26, 1.0)` = 0.26, and `roundToQuarter(0.26)` = 0.25.
        assert_eq!(borrowed[0].hours, 0.25);
    }

    /// C6, `BorrowerService.kt:61` — `meetingHours >= workHours`. Equal hours on both
    /// sides is meeting-heavy. A port typing `>` passes every other test in this file.
    #[tokio::test]
    async fn a_day_with_equal_meeting_and_work_hours_is_meeting_heavy() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0), work(date, "Coding", 1.0)];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 7200)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed.len(), 1, "6h short, meeting-heavy, so it borrows");
    }

    /// C6, `BorrowerService.kt:61`. One increment more work than meetings and the day
    /// is no longer meeting-heavy, whatever the shortfall.
    #[tokio::test]
    async fn a_day_with_more_work_than_meeting_hours_never_borrows() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0), work(date, "Coding", 1.25)];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 7200)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed, Vec::new());
        assert_eq!(history.calls(), Vec::new());
    }

    /// C6, `BorrowerService.kt:45-55`. Filler hours are counted twice and in opposite
    /// directions: they close the shortfall *and* they eat the synthetic budget. Here
    /// 2h of meetings plus 1h of filler leaves a 5h shortfall against a 3h remaining
    /// budget, so 3h is borrowed.
    #[tokio::test]
    async fn filler_hours_both_close_the_shortfall_and_shrink_the_budget() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 2.0)];
        let fillers = vec![filler(date, 1.0)];
        let history =
            FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 8 * 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed = borrow_for_meeting_only_days(
            &normalized,
            &fillers,
            &history,
            &config(),
            4.0,
            &normalizer,
        )
        .await
        .expect("borrow");

        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0].hours, 3.0, "4.0 budget minus 1.0 of filler");
    }

    /// C6, `BorrowerService.kt:49-50`. Filler hours enter `totalHours` and therefore
    /// shrink the **shortfall**, not only the budget. This fixture is the one where
    /// the two effects are distinguishable: the shortfall binds at 1.0h while 2.0h of
    /// budget is still free, so a port that leaves the fillers out of `totalHours`
    /// borrows 2.0h and lands the day at 9h.
    #[tokio::test]
    async fn filler_hours_shrink_the_shortfall_and_not_only_the_budget() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 5.0)];
        let fillers = vec![filler(date, 2.0)];
        let history =
            FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 8 * 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed = borrow_for_meeting_only_days(
            &normalized,
            &fillers,
            &history,
            &config(),
            4.0,
            &normalizer,
        )
        .await
        .expect("borrow");

        assert_eq!(borrowed.len(), 1);
        assert_eq!(
            borrowed[0].hours, 1.0,
            "5h of meetings plus 2h of filler leaves a 1h shortfall, not a 3h one"
        );
    }

    /// C6, `BorrowerService.kt:53-55`. A day whose fillers already used the whole
    /// synthetic budget has a remaining budget of zero, so the capped shortfall is
    /// zero and nothing is borrowed however short the day is.
    #[tokio::test]
    async fn a_fully_consumed_synthetic_budget_leaves_nothing_to_borrow() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];
        let fillers = vec![filler(date, 4.0)];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed = borrow_for_meeting_only_days(
            &normalized,
            &fillers,
            &history,
            &config(),
            4.0,
            &normalizer,
        )
        .await
        .expect("borrow");

        assert_eq!(borrowed, Vec::new());
        assert_eq!(history.calls(), Vec::new());
    }

    /// C6, `BorrowerService.kt:53-55`. Fillers past the budget push the remainder
    /// negative; `min` keeps it negative and the `> 0.25` gate rejects it. Ported
    /// because nothing clamps the remainder at zero.
    #[tokio::test]
    async fn an_overspent_synthetic_budget_goes_negative_rather_than_clamping() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];
        let fillers = vec![filler(date, 5.0)];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed = borrow_for_meeting_only_days(
            &normalized,
            &fillers,
            &history,
            &config(),
            4.0,
            &normalizer,
        )
        .await
        .expect("borrow");

        assert_eq!(borrowed, Vec::new());
    }

    /// C6, `BorrowerService.kt:55`. The shortfall, not the budget, can be the binding
    /// constraint: a 1.5h shortfall under a 4h budget borrows 1.5h.
    #[tokio::test]
    async fn the_shortfall_binds_when_it_is_below_the_remaining_budget() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 6.5)];
        let history =
            FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 8 * 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0].hours, 1.5);
    }

    /// C6, `BorrowerService.kt:50`. A day already at 8h has no shortfall at all.
    #[tokio::test]
    async fn a_day_already_at_eight_hours_borrows_nothing() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 8.0)];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed, Vec::new());
        assert_eq!(history.calls(), Vec::new());
    }

    /// `BorrowerService.kt:55` through [`java_min`]. A `max_synthetic_hours` of NaN
    /// makes the remaining budget NaN; `Math.min` propagates it and `NaN > 0.25` is
    /// false, so the incumbent borrows nothing. Rust's `f64::min` *discards* NaN and
    /// would return the raw 7h shortfall, which clears the gate — so this test is the
    /// whole reason `java_min` exists.
    #[tokio::test]
    async fn a_nan_synthetic_budget_borrows_nothing_rather_than_ignoring_the_cap() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];
        let history =
            FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 8 * 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed = borrow_for_meeting_only_days(
            &normalized,
            &[],
            &history,
            &config(),
            f64::NAN,
            &normalizer,
        )
        .await
        .expect("borrow");

        assert_eq!(borrowed, Vec::new());
        assert_eq!(history.calls(), Vec::new());
    }

    // -----------------------------------------------------------------------
    // The history window — `BorrowerService.kt:76-92`
    // -----------------------------------------------------------------------

    /// C6, `BorrowerService.kt:77-81`. The **fetch** window is
    /// `[earliest-7, earliest]` — `historyEndDate.plusDays(1)` is `earliestDate`
    /// itself, C2's UTC-axis padding. Off by one in either direction and a late
    /// evening entry is lost or a future day is pulled in.
    #[tokio::test]
    async fn the_history_fetch_runs_from_seven_days_back_to_the_earliest_day_itself() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Past work", 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
            .await
            .expect("borrow");

        assert_eq!(history.calls(), vec![(d(2026, 9, 11), d(2026, 9, 18))]);
    }

    /// C6, `BorrowerService.kt:78` against `BorrowerService.kt:92`. The two bounds differ
    /// by a day on purpose: the padding day is fetched and then dropped by the aggregate
    /// filter, so an entry dated `earliestDate` is never borrowable. A port that passes
    /// `historyEndDate.plusDays(1)` to both would borrow from the shortfall day itself.
    #[tokio::test]
    async fn an_entry_on_the_padding_day_is_fetched_and_then_filtered_out() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];
        let history = FixtureHistory::new(vec![
            chrono_entry(1, d(2026, 9, 18), "Same-day work", 4 * 3600),
            chrono_entry(2, d(2026, 9, 17), "Yesterday work", 2 * 3600),
        ]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(history.calls(), vec![(d(2026, 9, 11), d(2026, 9, 18))]);
        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Yesterday work"]);
    }

    /// C6, `BorrowerService.kt:77`. The far bound is inclusive: an entry exactly
    /// seven days before the earliest day is borrowable, one eight days before is not.
    #[tokio::test]
    async fn the_seventh_day_back_is_inside_the_window_and_the_eighth_is_not() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];
        let history = FixtureHistory::new(vec![
            chrono_entry(1, d(2026, 9, 10), "Eight days back", 4 * 3600),
            chrono_entry(2, d(2026, 9, 11), "Seven days back", 2 * 3600),
        ]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Seven days back"]);
    }

    /// C6, `BorrowerService.kt:76` — the incumbent's oddest line, ported verbatim.
    /// The anchor is `byDate.keys.minOrNull()`, the minimum over **every** day in the
    /// run, including days that are perfectly full. So a shortfall on 2026-09-30
    /// borrows from the week before 2026-09-01, not from the week before itself.
    /// A "fix" that anchors per shortfall day changes which tasks land in the system
    /// of record.
    #[tokio::test]
    async fn the_lookback_is_anchored_to_the_earliest_day_of_the_run_not_to_the_short_day() {
        let full_day = d(2026, 9, 1);
        let short_day = d(2026, 9, 30);
        let normalized = vec![
            meeting(full_day, "Full day", 8.0),
            meeting(short_day, "Standup", 1.0),
        ];
        let history = FixtureHistory::new(vec![chrono_entry(
            1,
            d(2026, 8, 28),
            "Late August work",
            4 * 3600,
        )]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(
            history.calls(),
            vec![(d(2026, 8, 25), d(2026, 9, 1))],
            "anchored to 2026-09-01, the earliest day present, not to 2026-09-30"
        );
        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0].date, short_day);
        assert_eq!(borrowed[0].source_date, d(2026, 8, 28));
    }

    /// `BorrowerService.kt:76`. The anchor is the earliest date, not the first one in
    /// input order — the groups are in encounter order and `minOrNull` scans them.
    #[tokio::test]
    async fn the_anchor_is_the_minimum_date_not_the_first_one_encountered() {
        let normalized = vec![
            meeting(d(2026, 9, 20), "Standup", 1.0),
            meeting(d(2026, 9, 10), "Full day", 8.0),
        ];
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 5), "Work", 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
            .await
            .expect("borrow");

        assert_eq!(history.calls(), vec![(d(2026, 9, 3), d(2026, 9, 10))]);
    }

    // -----------------------------------------------------------------------
    // Failure and emptiness — `BorrowerService.kt:80-98`
    // -----------------------------------------------------------------------

    /// C6's last rule, `BorrowerService.kt:80-85`: every exception out of the history
    /// fetch is swallowed and the answer is "no borrowing", not an error and not a
    /// message. Untestable against the incumbent without a mock HTTP layer, which is
    /// why [`HistorySource`] exists.
    #[tokio::test]
    async fn a_failing_history_fetch_yields_no_borrowing_rather_than_an_error() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];
        let history = FailingHistory::new();
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("a failed fetch must not surface as an error");

        assert_eq!(borrowed, Vec::new());
        assert_eq!(
            *history.calls.lock().expect("lock"),
            1,
            "the fetch was attempted exactly once and not retried"
        );
    }

    /// `BorrowerService.kt:87-89`. An empty history returns before the aggregation,
    /// which matters because `Aggregator.aggregate` on an unmapped project errors.
    #[tokio::test]
    async fn an_empty_history_yields_no_borrowing() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];
        let history = FixtureHistory::new(Vec::new());
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed, Vec::new());
    }

    /// C6, `BorrowerService.kt:96-98`. Meetings are filtered out of the borrowable
    /// set — borrowing a meeting would invent attendance. Here the whole history is
    /// an `Operations -` project, which C26 makes a meeting without touching disk.
    #[tokio::test]
    async fn a_history_of_nothing_but_meetings_yields_no_borrowing() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];

        let mut entry = chrono_entry(1, d(2026, 9, 15), "Ops sync", 4 * 3600);
        entry.project = Some(crate::model::ChronoProject {
            id: 2,
            name: "Operations - DevPro - Work".to_string(),
            color: "#fff".to_string(),
            aspect: None,
        });
        let history = FixtureHistory::new(vec![entry]);

        let mut config = config();
        config.mappings.push(ProjectMapping {
            chrono_project: "Operations - DevPro - Work".to_string(),
            devpro_project: DEVPRO_PROJECT.to_string(),
            billability: "NonBillable".to_string(),
        });

        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config, 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed, Vec::new());
    }

    /// C6, `BorrowerService.kt:96-98`. The complement of the test above: the meeting
    /// is dropped and the non-meeting beside it survives, so the filter is a filter
    /// and not a blanket bail-out.
    #[tokio::test]
    async fn meetings_are_dropped_from_the_history_while_work_beside_them_survives() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];

        let mut ops = chrono_entry(1, d(2026, 9, 15), "Ops sync", 6 * 3600);
        ops.project = Some(crate::model::ChronoProject {
            id: 2,
            name: "Operations - DevPro - Work".to_string(),
            color: "#fff".to_string(),
            aspect: None,
        });
        let history = FixtureHistory::new(vec![
            ops,
            chrono_entry(2, d(2026, 9, 15), "Real work", 2 * 3600),
        ]);

        let mut config = config();
        config.mappings.push(ProjectMapping {
            chrono_project: "Operations - DevPro - Work".to_string(),
            devpro_project: DEVPRO_PROJECT.to_string(),
            billability: "NonBillable".to_string(),
        });

        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config, 4.0, &normalizer)
                .await
                .expect("borrow");

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Real work"]);
    }

    /// `BorrowerService.kt:42-73`. An empty input never reaches the fetch, so the
    /// `minOrNull` fallback at `BorrowerService.kt:76` is unreachable — recorded here rather than
    /// asserted as behaviour it does not have.
    #[tokio::test]
    async fn an_empty_normalized_list_borrows_nothing_and_never_fetches() {
        let history = FixtureHistory::new(vec![chrono_entry(1, d(2026, 9, 15), "Work", 3600)]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&[], &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(borrowed, Vec::new());
        assert_eq!(history.calls(), Vec::new());
    }

    /// `BorrowerService.kt:92`. The aggregation sits outside the `try`, so an
    /// unmapped Chrono project in the *history* window propagates as an error rather
    /// than being swallowed like a fetch failure. The two failure modes are next to
    /// each other in the source and behave oppositely.
    #[tokio::test]
    async fn an_unmapped_project_in_the_history_is_an_error_not_a_silent_skip() {
        let date = d(2026, 9, 18);
        let normalized = vec![meeting(date, "Standup", 1.0)];

        let mut entry = chrono_entry(1, d(2026, 9, 15), "Mystery work", 3600);
        entry.project = Some(crate::model::ChronoProject {
            id: 9,
            name: "Unknown - DevPro - Work".to_string(),
            color: "#fff".to_string(),
            aspect: None,
        });
        let history = FixtureHistory::new(vec![entry]);
        let (_dir, normalizer) = offline_normalizer();

        let error =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect_err("an unmapped project must not be swallowed");

        assert!(
            error.to_string().contains("Unknown - DevPro - Work"),
            "unexpected error: {error}"
        );
    }

    // -----------------------------------------------------------------------
    // `borrow_for_day` — frequency, order, dedupe, budget
    // -----------------------------------------------------------------------

    const TARGET: fn() -> NaiveDate = || NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();

    /// C28 at `BorrowerService.kt:118-129`, the last-wins write at
    /// `BorrowerService.kt:122-123`. One description under two aggregates accumulates
    /// **both** shares while its project, billability and source date come from the
    /// **second**. The idiomatic `entry().or_insert()` keeps the first and would post
    /// these hours to the wrong project.
    #[test]
    fn a_description_under_two_aggregates_sums_the_hours_and_keeps_the_last_aggregate() {
        let first = agg(
            d(2026, 9, 14),
            CHRONO_PROJECT,
            1.0,
            &["Shared task"],
            "Alpha",
            "NonBillable",
        );
        let second = agg(
            d(2026, 9, 15),
            CHRONO_PROJECT,
            2.0,
            &["Shared task"],
            "Beta",
            "Billable",
        );

        let borrowed = borrow_for_day(TARGET(), 4.0, &[first, second]);

        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0].devpro_project_name, "Beta");
        assert_eq!(borrowed[0].billability, "Billable");
        assert_eq!(borrowed[0].source_date, d(2026, 9, 15));
        // The hours come from the *last* aggregate alone (2.0), not from the summed
        // frequency (3.0) — `BorrowerService.kt:155` reads `sourceAggregate.totalHours`.
        assert_eq!(borrowed[0].hours, 2.0);
    }

    /// C28 at `BorrowerService.kt:118`, the accumulation itself. The 0.5h task seen
    /// three times outranks the 1.0h task seen once, so it is borrowed first —
    /// a frequency map that overwrote instead of adding would reverse the order.
    #[test]
    fn accumulated_frequency_outranks_a_single_larger_aggregate() {
        let repeated = |date: NaiveDate| task(date, "Repeated", 0.5);
        let aggregates = vec![
            repeated(d(2026, 9, 14)),
            task(d(2026, 9, 14), "One-off", 1.0),
            repeated(d(2026, 9, 15)),
            repeated(d(2026, 9, 16)),
        ];

        let borrowed = borrow_for_day(TARGET(), 4.0, &aggregates);

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Repeated", "One-off"]);
        // 1.5h of accumulated frequency, but the hours still come from the last
        // aggregate carrying the key.
        assert_eq!(borrowed[0].hours, 0.5);
    }

    /// C28 at `BorrowerService.kt:128` — the stable sort. Three tasks tie at 0.5h of
    /// accumulated frequency and the budget only admits two, so insertion order
    /// decides *which two* get borrowed. A `HashMap` for `taskFrequency` makes this
    /// answer differ between processes.
    #[test]
    fn a_tie_in_frequency_is_broken_by_first_encounter_order() {
        let aggregates = vec![
            task(d(2026, 9, 14), "First seen", 0.5),
            task(d(2026, 9, 15), "Second seen", 0.5),
            task(d(2026, 9, 16), "Third seen", 0.5),
        ];

        let borrowed = borrow_for_day(TARGET(), 1.0, &aggregates);

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["First seen", "Second seen"]);
    }

    /// C28 at `BorrowerService.kt:128`, at a size where an unstable sort has room to
    /// move. Forty tasks tie at 0.25h and the budget admits three; the answer must be
    /// the first three in encounter order. `slice::sort_unstable_by` runs insertion
    /// sort below twenty elements and would agree on a smaller fixture by luck.
    #[test]
    fn a_forty_way_tie_still_borrows_the_first_three_in_encounter_order() {
        let aggregates: Vec<DayProjectAggregate> = (0..40)
            .map(|i| task(d(2026, 9, 14), &format!("Task {i:02}"), 0.25))
            .collect();

        let borrowed = borrow_for_day(TARGET(), 0.75, &aggregates);

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Task 00", "Task 01", "Task 02"]);
    }

    /// C28 at `BorrowerService.kt:128`, at the shape where a stable and an unstable
    /// sort actually part company. Sixty tasks in two tie blocks, the cheap block
    /// encountered first, so the descending sort has real work to do: an all-equal or
    /// already-descending input takes `sort_unstable_by`'s "already sorted" early exit
    /// and agrees with the stable sort by luck. The budget admits three, so the answer
    /// is which three of the thirty tied 0.5h tasks were seen first.
    #[test]
    fn a_tie_block_that_the_sort_must_actually_move_keeps_its_encounter_order() {
        let mut aggregates: Vec<DayProjectAggregate> = (0..30)
            .map(|i| task(d(2026, 9, 14), &format!("Cheap {i:02}"), 0.25))
            .collect();
        aggregates.extend((0..30).map(|i| task(d(2026, 9, 15), &format!("Dear {i:02}"), 0.5)));

        let borrowed = borrow_for_day(TARGET(), 1.5, &aggregates);

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Dear 00", "Dear 01", "Dear 02"]);
    }

    /// C28 at `BorrowerService.kt:128`. Descending, not ascending: the biggest block
    /// of hours is borrowed first, which is what makes the budget run out on the
    /// small tasks rather than the large ones.
    #[test]
    fn tasks_are_offered_in_descending_order_of_accumulated_hours() {
        let aggregates = vec![
            task(d(2026, 9, 14), "Small", 0.5),
            task(d(2026, 9, 15), "Large", 3.0),
            task(d(2026, 9, 16), "Medium", 1.0),
        ];

        let borrowed = borrow_for_day(TARGET(), 8.0, &aggregates);

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Large", "Medium", "Small"]);
    }

    /// C12 at its secondary site, `BorrowerService.kt:144-148`. The dedupe key is the
    /// **cleaned** title, so two different raw descriptions that clean to the same
    /// thing collapse into one borrowed entry. Deduping on the raw description would
    /// borrow both and log the same title twice on one day.
    #[test]
    fn the_dedupe_key_is_the_cleaned_title_and_not_the_raw_description() {
        let aggregates = vec![
            task(d(2026, 9, 14), "Weekly review", 3.0),
            task(
                d(2026, 9, 15),
                &format!("Weekly review - {CHRONO_PROJECT}"),
                2.0,
            ),
        ];

        let borrowed = borrow_for_day(TARGET(), 8.0, &aggregates);

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Weekly review"]);
    }

    /// C12, `BorrowerService.kt:145-148`. The trailing `, Mon D YYYY` is stripped, so
    /// the same meeting-shaped description on two dates is one borrowable title.
    #[test]
    fn a_trailing_date_is_stripped_before_the_title_is_deduped() {
        let aggregates = vec![
            task(d(2026, 9, 14), "Design sync, Sep 14 2026", 3.0),
            task(d(2026, 9, 15), "Design sync, Sep 15 2026", 2.0),
        ];

        let borrowed = borrow_for_day(TARGET(), 8.0, &aggregates);

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Design sync"]);
    }

    /// C12's two-stage composition, measured on JDK 21 and reproduced here through
    /// `clean_task_title`: a trailing newline makes `removeSuffix` miss, the
    /// unremoved suffix pushes the date out of anchor position, and **neither** stage
    /// cleans. The borrowed title therefore carries the whole raw description.
    #[test]
    fn a_trailing_newline_defeats_both_cleaning_stages_at_once() {
        let aggregates = vec![task(
            d(2026, 9, 14),
            &format!("Design sync, Sep 14 2026 - {CHRONO_PROJECT}\n"),
            3.0,
        )];

        let borrowed = borrow_for_day(TARGET(), 8.0, &aggregates);

        assert_eq!(
            borrowed[0].task_title,
            format!("Design sync, Sep 14 2026 - {CHRONO_PROJECT}\n")
        );
    }

    /// `BorrowerService.kt:141` against `BorrowerService.kt:151`. `taskIndex` is advanced
    /// *before* the dedupe `continue`, so a skipped duplicate does not stall the walk and
    /// the task behind it is still reached.
    #[test]
    fn a_deduped_task_does_not_block_the_one_behind_it() {
        let aggregates = vec![
            task(d(2026, 9, 14), "Weekly review", 3.0),
            task(
                d(2026, 9, 15),
                &format!("Weekly review - {CHRONO_PROJECT}"),
                2.0,
            ),
            task(d(2026, 9, 16), "Something else", 1.0),
        ];

        let borrowed = borrow_for_day(TARGET(), 8.0, &aggregates);

        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["Weekly review", "Something else"]);
    }

    /// `BorrowerService.kt:151-158`. The title is marked used **before** the
    /// `hours > 0` test, so a task whose share rounds to zero still burns its cleaned
    /// title for the rest of the day and the later, larger task carrying the same
    /// title is skipped entirely.
    ///
    /// The fixture leans on the last-wins write to get a high frequency with a tiny
    /// payout: `"Weekly review - <project>"` accumulates 5.0 + 0.1 = 5.1h and so
    /// sorts first, while `BorrowerService.kt:155` reads only the last aggregate's 0.1h, which
    /// `roundToQuarter` sends to zero.
    #[test]
    fn a_zero_hour_task_still_burns_its_title_for_the_rest_of_the_day() {
        let suffixed = format!("Weekly review - {CHRONO_PROJECT}");
        let aggregates = vec![
            task(d(2026, 9, 14), &suffixed, 5.0),
            task(d(2026, 9, 15), &suffixed, 0.1),
            task(d(2026, 9, 16), "Weekly review", 2.0),
        ];

        let borrowed = borrow_for_day(TARGET(), 4.0, &aggregates);

        assert_eq!(
            borrowed,
            Vec::new(),
            "the 0.1h task takes the title and pays nothing; the 2.0h task is then deduped away"
        );
    }

    /// C27 through `BorrowerService.kt:155-156` — the cap is applied before the
    /// rounding, so the hours handed out can **exceed** the shortfall. A 0.4h
    /// remainder yields 0.5h. This is the incumbent's arithmetic, not a defect to
    /// clamp: `roundToQuarter(min(x, 0.4))` is `roundToQuarter(0.4)` is `0.5`.
    #[test]
    fn rounding_after_the_cap_can_hand_out_more_hours_than_the_shortfall() {
        let aggregates = vec![task(d(2026, 9, 14), "Big task", 3.0)];

        let borrowed = borrow_for_day(TARGET(), 0.4, &aggregates);

        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0].hours, 0.5);
    }

    /// C27 at `BorrowerService.kt:175`. `roundToQuarter` **rounds**, unlike the five
    /// truncating `toInt()` sites in `SettleCommand.kt`: `0.375` becomes `0.5` here
    /// and would become `0.25` there. A port unifying the two regimes passes every
    /// other test in this file.
    #[test]
    fn the_borrowers_quantizer_rounds_rather_than_truncating() {
        let aggregates = vec![task(d(2026, 9, 14), "Odd task", 0.375)];

        let borrowed = borrow_for_day(TARGET(), 8.0, &aggregates);

        assert_eq!(borrowed[0].hours, 0.5);
    }

    /// `BorrowerService.kt:139`. The loop condition is `>=`, so a remaining fill of
    /// exactly one increment still runs a pass and borrows 0.25h.
    #[test]
    fn a_remaining_fill_of_exactly_one_increment_still_borrows() {
        let aggregates = vec![task(d(2026, 9, 14), "Big task", 3.0)];

        let borrowed = borrow_for_day(TARGET(), HOUR_INCREMENT, &aggregates);

        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed[0].hours, 0.25);
    }

    /// `BorrowerService.kt:139`, the other side. Below one increment the loop never
    /// starts, so nothing is borrowed however much history is available.
    #[test]
    fn a_remaining_fill_below_one_increment_borrows_nothing() {
        let aggregates = vec![task(d(2026, 9, 14), "Big task", 3.0)];

        let borrowed = borrow_for_day(TARGET(), 0.24, &aggregates);

        assert_eq!(borrowed, Vec::new());
    }

    /// `BorrowerService.kt:134,167`. The budget runs down by the hours actually
    /// handed out, and the walk stops as soon as it drops below an increment — the
    /// tasks behind it are left untouched even though they exist.
    #[test]
    fn the_walk_stops_as_soon_as_the_budget_drops_below_an_increment() {
        let aggregates = vec![
            task(d(2026, 9, 14), "First", 1.0),
            task(d(2026, 9, 15), "Second", 0.5),
            task(d(2026, 9, 16), "Third", 0.5),
        ];

        let borrowed = borrow_for_day(TARGET(), 1.25, &aggregates);

        let pairs: Vec<(&str, f64)> = borrowed
            .iter()
            .map(|b| (b.task_title.as_str(), b.hours))
            .collect();
        assert_eq!(pairs, vec![("First", 1.0), ("Second", 0.25)]);
    }

    /// `BorrowerService.kt:112`. No history, no borrowing — the guard before the
    /// frequency map.
    #[test]
    fn an_empty_historical_set_borrows_nothing() {
        assert_eq!(borrow_for_day(TARGET(), 4.0, &[]), Vec::new());
    }

    /// `BorrowerService.kt:120,131`. An aggregate with no descriptions contributes no
    /// key at all, so `sortedTasks` stays empty and the `coerceAtLeast(1)` guard at
    /// `BorrowerService.kt:123` is never the thing standing between this and a division by zero.
    #[test]
    fn aggregates_without_descriptions_contribute_no_borrowable_task() {
        let aggregates = vec![agg(
            d(2026, 9, 14),
            CHRONO_PROJECT,
            4.0,
            &[],
            DEVPRO_PROJECT,
            "NonBillable",
        )];

        assert_eq!(borrow_for_day(TARGET(), 4.0, &aggregates), Vec::new());
    }

    /// `BorrowerService.kt:123,155`. Both the frequency share and the payout divide
    /// the aggregate's hours by its description count, so a four-description, 4h
    /// aggregate offers 1h per title. `Aggregator` never builds one — it keys on the
    /// description, so `descriptions` holds at most one element (C11) — which makes
    /// this a synthetic case covering a live code path rather than an observed one.
    #[test]
    fn a_multi_description_aggregate_splits_its_hours_evenly() {
        let aggregates = vec![agg(
            d(2026, 9, 14),
            CHRONO_PROJECT,
            4.0,
            &["One", "Two", "Three", "Four"],
            DEVPRO_PROJECT,
            "NonBillable",
        )];

        let borrowed = borrow_for_day(TARGET(), 8.0, &aggregates);

        assert_eq!(borrowed.len(), 4);
        for entry in &borrowed {
            assert_eq!(entry.hours, 1.0);
        }
        let titles: Vec<&str> = borrowed.iter().map(|b| b.task_title.as_str()).collect();
        assert_eq!(titles, vec!["One", "Two", "Three", "Four"]);
    }

    /// `BorrowerService.kt:159-166`. Every field but `date` and `hours` is copied off
    /// the source aggregate, and `date` is the day being filled — not the day the
    /// task came from. Swapping the two would file last week's hours on last week.
    #[test]
    fn a_borrowed_entry_carries_the_target_day_and_the_sources_identity() {
        let aggregates = vec![agg(
            d(2026, 9, 14),
            CHRONO_PROJECT,
            1.0,
            &["Some task"],
            "Inveniam SOW #5",
            "Billable",
        )];

        let borrowed = borrow_for_day(TARGET(), 4.0, &aggregates);

        assert_eq!(
            borrowed[0],
            BorrowedEntry {
                date: TARGET(),
                source_date: d(2026, 9, 14),
                devpro_project_name: "Inveniam SOW #5".to_string(),
                task_title: "Some task".to_string(),
                billability: "Billable".to_string(),
                hours: 1.0,
            }
        );
    }

    // -----------------------------------------------------------------------
    // Multi-day behaviour and the captured run
    // -----------------------------------------------------------------------

    /// `BorrowerService.kt:101-103,136`. `usedTaskTitles` is created inside
    /// `borrowForDay`, so the dedupe is per day: the same task is borrowed onto two
    /// short days. Hoisting the set out of the loop would silently halve the second
    /// day's borrowing.
    #[tokio::test]
    async fn the_title_dedupe_resets_between_days() {
        let normalized = vec![
            meeting(d(2026, 9, 17), "Standup", 7.0),
            meeting(d(2026, 9, 18), "Standup", 7.0),
        ];
        let history = FixtureHistory::new(vec![chrono_entry(
            1,
            d(2026, 9, 14),
            "Reusable task",
            4 * 3600,
        )]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        let pairs: Vec<(NaiveDate, &str)> = borrowed
            .iter()
            .map(|b| (b.date, b.task_title.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (d(2026, 9, 17), "Reusable task"),
                (d(2026, 9, 18), "Reusable task"),
            ]
        );
    }

    /// C28 at `BorrowerService.kt:42-69,101`. Short days are emitted in the order
    /// their groups were first encountered, not in date order — `groupBy` and
    /// `toMap` are both `LinkedHashMap`s and `flatMap` walks them as they are.
    #[tokio::test]
    async fn short_days_are_emitted_in_first_encounter_order_not_date_order() {
        let normalized = vec![
            meeting(d(2026, 9, 18), "Standup", 7.0),
            meeting(d(2026, 9, 17), "Standup", 7.0),
        ];
        let history = FixtureHistory::new(vec![chrono_entry(
            1,
            d(2026, 9, 12),
            "Reusable task",
            4 * 3600,
        )]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        let dates: Vec<NaiveDate> = borrowed.iter().map(|b| b.date).collect();
        assert_eq!(dates, vec![d(2026, 9, 18), d(2026, 9, 17)]);
    }

    /// C28's cheap guard: the same input twice must give byte-identical output. This
    /// fails immediately if a randomized `HashMap` ever reaches `taskFrequency`,
    /// which no single-run assertion can catch.
    #[test]
    fn the_same_input_borrows_the_same_way_twice() {
        let aggregates: Vec<DayProjectAggregate> = (0..40)
            .map(|i| task(d(2026, 9, 14), &format!("Task {i:02}"), 0.5))
            .collect();

        let first = borrow_for_day(TARGET(), 2.0, &aggregates);
        let second = borrow_for_day(TARGET(), 2.0, &aggregates);

        assert_eq!(first, second);
        assert_eq!(first.len(), 4);
    }

    /// C6 end to end against the captured incumbent run,
    /// `~/.cache/tt-devpro-rewrite/baseline/settle-json.out` for 2026-09-18: five
    /// borrowed entries totalling exactly 4.0h, the default `max_synthetic_hours`,
    /// against a day holding 3.0h of meetings and no work.
    ///
    /// The source hours are reconstructed from the capture's borrowed hours (each is
    /// a quarter multiple that the cap never bound, except the last), and the source
    /// dates are the capture's `sourceDate` values verbatim.
    ///
    /// What the capture witnesses is the *set*: five titles, their hours, their source
    /// dates, and the 4.0h total. It does not witness the row order asserted below —
    /// the capture's JSON is grouped by project, which is the renderer's doing,
    /// downstream of `borrow_for_day` — and the order is not what lands the total on
    /// 4.0h either: `"Sync w Ivan"`, `"Analyze the Cursor usage export…"` and
    /// `"Program management discussion"` all tie at 0.5h and all three fit, while
    /// `"Send Omar…"` at 0.25h sorts last under every permutation. The order is
    /// asserted here as a consequence of the stable sort, and the sort's tie-break is
    /// pinned where it actually decides an outcome, in
    /// `a_tie_in_frequency_is_broken_by_first_encounter_order` and
    /// `a_tie_block_that_the_sort_must_actually_move_keeps_its_encounter_order`.
    #[test]
    fn the_captured_2026_09_18_day_borrows_the_five_entries_the_incumbent_logged() {
        let inveniam = "Inveniam Measurabl - Presales - DevPro - Work";
        // Insertion order = `TimeNormalizer.normalize`'s sort by (date, project).
        let aggregates = vec![
            agg(
                d(2026, 9, 14),
                CHRONO_PROJECT,
                0.5,
                &["Sync w Ivan about SWE agents"],
                "Delivery Practices",
                "NonBillable",
            ),
            agg(
                d(2026, 9, 14),
                inveniam,
                0.25,
                &["Send Omar the AWS IAM user and verify Snowflake and S3 access"],
                "Inveniam SOW #5",
                "Billable",
            ),
            agg(
                d(2026, 9, 15),
                inveniam,
                0.5,
                &["Analyze the Cursor usage export and build the token-governance case"],
                "Inveniam SOW #5",
                "Billable",
            ),
            agg(
                d(2026, 9, 15),
                inveniam,
                0.5,
                &["Program management discussion"],
                "Inveniam SOW #5",
                "Billable",
            ),
            agg(
                d(2026, 9, 17),
                inveniam,
                2.25,
                &["Map every Connect issue to its producer and its owner"],
                "Inveniam SOW #5",
                "Billable",
            ),
        ];

        // 8.0 - 3.0 of meetings = 5.0 shortfall, capped at the 4.0 budget.
        let borrowed = borrow_for_day(TARGET(), 4.0, &aggregates);

        let rows: Vec<(&str, f64, NaiveDate)> = borrowed
            .iter()
            .map(|b| (b.task_title.as_str(), b.hours, b.source_date))
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "Map every Connect issue to its producer and its owner",
                    2.25,
                    d(2026, 9, 17)
                ),
                ("Sync w Ivan about SWE agents", 0.5, d(2026, 9, 14)),
                (
                    "Analyze the Cursor usage export and build the token-governance case",
                    0.5,
                    d(2026, 9, 15)
                ),
                ("Program management discussion", 0.5, d(2026, 9, 15)),
                (
                    "Send Omar the AWS IAM user and verify Snowflake and S3 access",
                    0.25,
                    d(2026, 9, 14)
                ),
            ]
        );
        let total: f64 = borrowed.iter().map(|b| b.hours).sum();
        assert_eq!(total, 4.0, "the capture's 'borrowed+filler cap reached'");
    }

    /// The same captured day driven through the whole entry point rather than through
    /// `borrow_for_day`, so the day gate, the window arithmetic and the meeting filter
    /// are all in the path. 3.0h of meetings, no work, no fillers, default budget.
    #[tokio::test]
    async fn the_captured_2026_09_18_day_borrows_four_hours_end_to_end() {
        let date = d(2026, 9, 18);
        let normalized = vec![
            meeting(date, "AI Heads Sync", 0.5),
            meeting(date, "SDLC Tools Repository Sync", 0.5),
            meeting(date, "Sentinel - next steps", 1.0),
            meeting(date, "Connect Ingestion tech deep dive", 1.0),
        ];
        let history = FixtureHistory::new(vec![
            chrono_entry(1, d(2026, 9, 17), "Map every Connect issue", 8100),
            chrono_entry(2, d(2026, 9, 15), "Analyze the Cursor usage export", 1800),
            chrono_entry(3, d(2026, 9, 15), "Program management discussion", 1800),
            chrono_entry(4, d(2026, 9, 14), "Sync w Ivan about SWE agents", 1800),
            chrono_entry(5, d(2026, 9, 14), "Send Omar the AWS IAM user", 900),
        ]);
        let (_dir, normalizer) = offline_normalizer();

        let borrowed =
            borrow_for_meeting_only_days(&normalized, &[], &history, &config(), 4.0, &normalizer)
                .await
                .expect("borrow");

        assert_eq!(history.calls(), vec![(d(2026, 9, 11), d(2026, 9, 18))]);
        assert_eq!(borrowed.len(), 5);
        let total: f64 = borrowed.iter().map(|b| b.hours).sum();
        assert_eq!(total, 4.0);
        assert!(borrowed.iter().all(|b| b.date == date));
        assert_eq!(borrowed[0].task_title, "Map every Connect issue");
        assert_eq!(borrowed[0].hours, 2.25);
    }
}
