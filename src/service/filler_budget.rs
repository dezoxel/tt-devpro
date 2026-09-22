//! Filler budgets across billing periods — the port of `service/FillerBudgetService.kt`.
//!
//! Billing periods are the 1st–15th and the 16th–end of each month (C6). A filler
//! configured with `max_hours_per_period` may spend at most that many hours inside
//! one period, and hours already sitting in the portal for the same
//! `(projectShortName, taskTitle)` come off that allowance first.
//!
//! Two things about this file are worth reading before changing it.
//!
//! **The budget map is keyed, never iterated.** The incumbent's `mutableMapOf` is a
//! `LinkedHashMap` and C28 makes map iteration order load-bearing in five places —
//! none of them is here. `calculateRemainingBudgets` returns a map that only
//! `hasRemainingBudget` and `consumeBudget` ever touch, and both index it by key
//! (`FillerBudgetService.kt:105,119`). A plain `HashMap` is therefore exact rather
//! than merely convenient, and `FillerKey` derives `Hash`/`Eq` and not `Ord`, so a
//! `BTreeMap` would not be available anyway.
//!
//! **`PeriodBudgets` is D2's third facet and is a deliberate divergence from the
//! incumbent.** `SettleCommand.kt:449-456` enumerates every billing period the
//! settle range spans and then builds one budget map out of the *first* of them —
//! the comment on the line says "most common case" out loud. A range crossing a
//! boundary (`--from 2026-08-10 --to 2026-09-05` spans three periods) therefore
//! checks every day's filler against hours that were never that day's to spend.
//! This port budgets per period and asks for the period of the day being filled.
//! It is the one place in the module with no parity risk at all: G4 established
//! that `max_hours_per_period` appears nowhere in the live `~/.tt-config.yaml`, so
//! `config.fillers.any { it.maxHoursPerPeriod != null }` is false on every real run
//! and the whole branch is dead code today. Everything else here is verbatim.

use std::collections::{BTreeMap, HashMap};

use chrono::{Datelike, NaiveDate};

use crate::config::Filler;
use crate::model::{FillerKey, WorklogDetail};

/// `Double.MAX_VALUE` — the incumbent's "no cap configured" sentinel
/// (`FillerBudgetService.kt:77,105,119`). Not an `Option`: the incumbent stores the
/// sentinel in the map itself and both readers compare against it, so an `Option`
/// here would change `consumeBudget`'s branch rather than tidy it.
pub const UNLIMITED: f64 = f64::MAX;

/// `FillerBudgetService.kt:102` — `hasRemainingBudget`'s default `minRequired`.
/// Rust has no default arguments, so the default becomes a named constant and the
/// one call site (`FillerService.kt:96`) passes it explicitly.
pub const DEFAULT_MIN_REQUIRED_HOURS: f64 = 0.25;

/// Remaining hours per filler, inside one billing period.
pub type Budgets = HashMap<FillerKey, f64>;

/// `FillerBudgetService.kt:18-25`.
///
/// `Ord` orders by `start` and then `end`, which is what `getBillingPeriodsInRange`
/// sorts by and what makes `PeriodBudgets`' `BTreeMap` iterate chronologically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BillingPeriod {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl BillingPeriod {
    /// `FillerBudgetService.kt:22-24` — `!date.isBefore(start) && !date.isAfter(end)`,
    /// inclusive at both ends.
    pub fn contains(&self, date: NaiveDate) -> bool {
        date >= self.start && date <= self.end
    }
}

/// `FillerBudgetService.kt:32-44` (`getBillingPeriod`). Days 1–15 form the first
/// period, 16–end the second.
///
/// Panics only for a date whose month has no representable successor, i.e. December
/// of `NaiveDate`'s maximum year — unreachable from any date this tool handles, and
/// the incumbent's `date.lengthOfMonth()` is total in the same way.
pub fn billing_period(date: NaiveDate) -> BillingPeriod {
    if date.day() <= 15 {
        BillingPeriod {
            start: date.with_day(1).expect("day 1 exists in every month"),
            end: date.with_day(15).expect("day 15 exists in every month"),
        }
    } else {
        BillingPeriod {
            start: date.with_day(16).expect("day 16 exists in every month"),
            end: last_day_of_month(date),
        }
    }
}

/// `FillerBudgetService.kt:49-57` (`getBillingPeriodsInRange`). Walks the range a
/// day at a time, collects the distinct periods, returns them sorted by `start`.
///
/// An inverted range (`start` after `end`) yields nothing, exactly as the Kotlin
/// `while (!current.isAfter(endDate))` does — which is the case
/// `SettleCommand.kt:452`'s `?:` fallback exists to cover.
pub fn billing_periods_in_range(start_date: NaiveDate, end_date: NaiveDate) -> Vec<BillingPeriod> {
    let mut periods: Vec<BillingPeriod> = Vec::new();
    let mut current = start_date;

    while current <= end_date {
        let period = billing_period(current);
        if !periods.contains(&period) {
            periods.push(period);
        }
        match current.succ_opt() {
            Some(next) => current = next,
            // `LocalDate.plusDays(1)` throws at the end of the supported range; the
            // loop ends here instead, which no reachable range gets near.
            None => break,
        }
    }

    // Kotlin's `sortedBy` is a stable TimSort (C28). Every period in the set has a
    // distinct `start`, so stability decides nothing here — the sort is total.
    periods.sort_by(|a, b| a.start.cmp(&b.start));
    periods
}

/// `FillerBudgetService.kt:67-93` (`calculateRemainingBudgets`).
///
/// Every filler starts at its configured cap, or at [`UNLIMITED`] when it has none.
/// Worklogs inside `billing_period` whose `(projectShortName, taskTitle)` matches a
/// filler then reduce that filler's allowance, floored at zero. An unlimited
/// allowance is never reduced (`:87` guards on `< Double.MAX_VALUE`), and a worklog
/// matching no filler creates no entry.
pub fn calculate_remaining_budgets(
    fillers: &[Filler],
    existing_worklogs: &[(WorklogDetail, NaiveDate)],
    billing_period: &BillingPeriod,
) -> Budgets {
    let mut budgets: Budgets = HashMap::new();

    // `:75-79`. Two fillers sharing a key is last-wins, as `LinkedHashMap.put` is.
    for filler in fillers {
        let key = FillerKey {
            devpro_project: filler.devpro_project.clone(),
            task_title: filler.task_title.clone(),
        };
        budgets.insert(key, filler.max_hours_per_period.unwrap_or(UNLIMITED));
    }

    // `:82-90`.
    for (worklog, date) in existing_worklogs {
        if !billing_period.contains(*date) {
            continue;
        }

        let key = FillerKey {
            devpro_project: worklog.project_short_name.clone(),
            task_title: worklog.task_title.clone(),
        };
        if let Some(&current_budget) = budgets.get(&key) {
            if current_budget < UNLIMITED {
                budgets.insert(key, 0.0_f64.max(current_budget - worklog.logged_hours));
            }
        }
    }

    budgets
}

/// `FillerBudgetService.kt:98-107` (`hasRemainingBudget`). A key with no entry is
/// unlimited, and the comparison is `>=`, so a budget sitting exactly on
/// `min_required` still counts as available.
pub fn has_remaining_budget(
    budgets: &Budgets,
    devpro_project: &str,
    task_title: &str,
    min_required: f64,
) -> bool {
    let key = FillerKey {
        devpro_project: devpro_project.to_string(),
        task_title: task_title.to_string(),
    };
    let remaining = budgets.get(&key).copied().unwrap_or(UNLIMITED);
    remaining >= min_required
}

/// `FillerBudgetService.kt:112-130` (`consumeBudget`). Returns what was actually
/// granted, which may be less than requested.
///
/// The `>= Double.MAX_VALUE - 1` test at `:121` is ported verbatim. In `f64` the
/// `- 1.0` changes nothing — one is far below `MAX`'s ULP — so the test is exactly
/// "is this the sentinel", and a merely enormous finite budget is consumed like any
/// other. An unlimited budget grants the full request and writes nothing back, so a
/// key that was absent stays absent.
pub fn consume_budget(
    budgets: &mut Budgets,
    devpro_project: &str,
    task_title: &str,
    requested_hours: f64,
) -> f64 {
    let key = FillerKey {
        devpro_project: devpro_project.to_string(),
        task_title: task_title.to_string(),
    };
    let available = budgets.get(&key).copied().unwrap_or(UNLIMITED);

    if available >= UNLIMITED - 1.0 {
        requested_hours
    } else {
        let consumed = requested_hours.min(available);
        budgets.insert(key, available - consumed);
        consumed
    }
}

/// D2's third facet: one budget map per billing period the settle range spans,
/// instead of the incumbent's single map built from the range's first period
/// (`SettleCommand.kt:449-456`).
///
/// The filler already receives the day it is filling (`FillerService.kt:44`), so it
/// asks this for that day's period rather than being handed one map for the whole
/// range.
#[derive(Debug, Clone, PartialEq)]
pub struct PeriodBudgets {
    by_period: BTreeMap<BillingPeriod, Budgets>,
}

impl PeriodBudgets {
    /// Builds one budget map per period, each reduced only by the worklogs that
    /// fall inside that period.
    pub fn calculate(
        fillers: &[Filler],
        existing_worklogs: &[(WorklogDetail, NaiveDate)],
        periods: &[BillingPeriod],
    ) -> Self {
        let by_period = periods
            .iter()
            .map(|period| {
                (
                    period.clone(),
                    calculate_remaining_budgets(fillers, existing_worklogs, period),
                )
            })
            .collect();
        Self { by_period }
    }

    /// The whole of `SettleCommand.kt:448-456`, D2's third facet applied: the
    /// `config.fillers.any { it.maxHoursPerPeriod != null }` gate, the period
    /// enumeration, and the budgets themselves.
    ///
    /// `None` means no filler configures a cap, which G4 says is every real run
    /// today — and the caller then passes no budgets at all, exactly as the
    /// incumbent's `else null` branch does.
    ///
    /// The gate and the enumeration live here rather than at the call site so the
    /// fix cannot be half-applied: enumerating the periods and then using one of
    /// them is precisely the defect.
    pub fn calculate_if_configured(
        fillers: &[Filler],
        existing_worklogs: &[(WorklogDetail, NaiveDate)],
        from: NaiveDate,
        to: NaiveDate,
    ) -> Option<Self> {
        if !fillers.iter().any(|f| f.max_hours_per_period.is_some()) {
            return None;
        }

        let mut periods = billing_periods_in_range(from, to);
        if periods.is_empty() {
            // `:452`'s `?: getBillingPeriod(from)` — an inverted range still gets a
            // period rather than an empty budget set.
            periods.push(billing_period(from));
        }

        Some(Self::calculate(fillers, existing_worklogs, &periods))
    }

    /// The budgets for the period holding `date`, or `None` when no period does.
    ///
    /// Scans rather than looking `billing_period(date)` up directly, so a caller
    /// that built this from a hand-made period set still gets the right map. The
    /// periods are disjoint, so at most one can match; `None` means "no budget
    /// known", which every reader here already treats as unlimited.
    ///
    /// Test-only, and not because the read is uninteresting: the production path
    /// needs `&mut` anyway (`has_remaining_budget` checks and `consume_budget`
    /// writes back through the same borrow), so `for_date_mut` serves it alone and
    /// a shared borrow has no caller left. Its job here is observation — the D2
    /// third-facet assertions read a period's map without consuming it.
    #[cfg(test)]
    pub fn for_date(&self, date: NaiveDate) -> Option<&Budgets> {
        self.by_period
            .iter()
            .find(|(period, _)| period.contains(date))
            .map(|(_, budgets)| budgets)
    }

    /// `for_date`, for the filler's consume path.
    pub fn for_date_mut(&mut self, date: NaiveDate) -> Option<&mut Budgets> {
        self.by_period
            .iter_mut()
            .find(|(period, _)| period.contains(date))
            .map(|(_, budgets)| budgets)
    }

    /// The periods this was built for, in chronological order.
    ///
    /// Test-only. The incumbent's period *list* is enumerated at
    /// `SettleCommand.kt:449` and then only `.firstOrNull()`'d at `:452` — D2's
    /// third facet moved that enumeration inside `calculate_if_configured`
    /// precisely so no caller can hold the list and pick one. Nothing in the port
    /// should want this; the tests want it, to prove the enumeration happened.
    #[cfg(test)]
    pub fn periods(&self) -> impl Iterator<Item = &BillingPeriod> {
        self.by_period.keys()
    }
}

/// `LocalDate.withDayOfMonth(date.lengthOfMonth())` — the last day of `date`'s
/// month, derived from `chrono`'s own calendar rather than a second leap-year rule.
fn last_day_of_month(date: NaiveDate) -> NaiveDate {
    let (year, month) = (date.year(), date.month());
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };

    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|first_of_next| first_of_next.pred_opt())
        .expect("every month this tool sees has a representable last day")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("test date literal")
    }

    fn period(start: &str, end: &str) -> BillingPeriod {
        BillingPeriod {
            start: date(start),
            end: date(end),
        }
    }

    fn filler(devpro_project: &str, task_title: &str, max_per_period: Option<f64>) -> Filler {
        Filler {
            devpro_project: devpro_project.to_string(),
            task_title: task_title.to_string(),
            billability: "NonBillable".to_string(),
            min_hours: 0.5,
            max_hours: 2.0,
            max_hours_per_period: max_per_period,
        }
    }

    fn worklog(project_short_name: &str, task_title: &str, logged_hours: f64) -> WorklogDetail {
        WorklogDetail {
            unique_id: format!("{project_short_name}/{task_title}/{logged_hours}"),
            project_unique_id: "project-uuid".to_string(),
            project_short_name: project_short_name.to_string(),
            task_title: task_title.to_string(),
            billability: "NonBillable".to_string(),
            logged_hours,
            is_deletable: true,
            expense_type: None,
        }
    }

    fn key(devpro_project: &str, task_title: &str) -> FillerKey {
        FillerKey {
            devpro_project: devpro_project.to_string(),
            task_title: task_title.to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // billing_period — the 1–15 / 16–end split (C6, `FillerBudgetService.kt:32-44`)
    // -----------------------------------------------------------------------

    /// C6. `FillerBudgetService.kt:33-37`.
    #[test]
    fn the_first_of_the_month_opens_the_first_half_period() {
        assert_eq!(
            billing_period(date("2026-09-01")),
            period("2026-09-01", "2026-09-15")
        );
    }

    /// C6. `FillerBudgetService.kt:33` — the branch is `dayOfMonth <= 15`, so the
    /// 15th is the last day of the first half and not the first day of the second.
    #[test]
    fn the_fifteenth_is_the_last_day_of_the_first_half_not_the_first_of_the_second() {
        assert_eq!(
            billing_period(date("2026-09-15")),
            period("2026-09-01", "2026-09-15")
        );
    }

    /// C6. `FillerBudgetService.kt:38-42`. The other side of the same boundary.
    #[test]
    fn the_sixteenth_opens_the_second_half_period() {
        assert_eq!(
            billing_period(date("2026-09-16")),
            period("2026-09-16", "2026-09-30")
        );
    }

    /// C6. `FillerBudgetService.kt:41` — `withDayOfMonth(lengthOfMonth())`. A port
    /// that hardcoded 30 or 31 passes one of these two and fails the other.
    #[test]
    fn the_second_half_ends_on_the_real_last_day_of_a_30_and_a_31_day_month() {
        assert_eq!(
            billing_period(date("2026-09-20")),
            period("2026-09-16", "2026-09-30")
        );
        assert_eq!(
            billing_period(date("2026-08-20")),
            period("2026-08-16", "2026-08-31")
        );
        // The last day itself resolves to its own period.
        assert_eq!(
            billing_period(date("2026-09-30")),
            period("2026-09-16", "2026-09-30")
        );
        assert_eq!(
            billing_period(date("2026-08-31")),
            period("2026-08-16", "2026-08-31")
        );
    }

    /// C6. February is the month where `lengthOfMonth()` earns its keep: 28 in a
    /// common year, 29 in a leap year, and the second period's end moves with it.
    #[test]
    fn february_ends_on_the_28th_in_a_common_year_and_the_29th_in_a_leap_year() {
        assert_eq!(
            billing_period(date("2026-02-20")),
            period("2026-02-16", "2026-02-28")
        );
        assert_eq!(
            billing_period(date("2026-02-28")),
            period("2026-02-16", "2026-02-28")
        );
        assert_eq!(
            billing_period(date("2024-02-17")),
            period("2024-02-16", "2024-02-29")
        );
        assert_eq!(
            billing_period(date("2024-02-29")),
            period("2024-02-16", "2024-02-29")
        );
    }

    /// C6. The century rule, which a hand-rolled `year % 4 == 0` gets wrong in both
    /// directions: 2000 is a leap year and 2100 is not.
    #[test]
    fn the_leap_year_century_rule_holds_in_both_directions() {
        assert_eq!(
            billing_period(date("2000-02-20")),
            period("2000-02-16", "2000-02-29")
        );
        assert_eq!(
            billing_period(date("2100-02-20")),
            period("2100-02-16", "2100-02-28")
        );
    }

    /// C6. The first half is 1–15 whatever the month's length, so February's first
    /// period is identical in a leap and a common year.
    #[test]
    fn the_first_half_is_1_to_15_regardless_of_month_length() {
        assert_eq!(
            billing_period(date("2024-02-15")),
            period("2024-02-01", "2024-02-15")
        );
        assert_eq!(
            billing_period(date("2026-02-15")),
            period("2026-02-01", "2026-02-15")
        );
        assert_eq!(
            billing_period(date("2026-04-15")),
            period("2026-04-01", "2026-04-15")
        );
        assert_eq!(
            billing_period(date("2026-12-15")),
            period("2026-12-01", "2026-12-15")
        );
    }

    /// C6. December's second half must end on the 31st and stay inside its own
    /// year — the one month where the last-day derivation rolls the year over.
    #[test]
    fn december_second_half_ends_on_the_31st_of_the_same_year() {
        assert_eq!(
            billing_period(date("2026-12-31")),
            period("2026-12-16", "2026-12-31")
        );
    }

    /// C6. March and April sit either side of February and are the months an
    /// off-by-one in the next-month derivation would corrupt.
    #[test]
    fn march_and_april_second_halves_end_on_the_31st_and_the_30th() {
        assert_eq!(
            billing_period(date("2026-03-16")),
            period("2026-03-16", "2026-03-31")
        );
        assert_eq!(
            billing_period(date("2026-03-31")),
            period("2026-03-16", "2026-03-31")
        );
        assert_eq!(
            billing_period(date("2026-04-30")),
            period("2026-04-16", "2026-04-30")
        );
        assert_eq!(
            billing_period(date("2024-03-31")),
            period("2024-03-16", "2024-03-31")
        );
    }

    // -----------------------------------------------------------------------
    // BillingPeriod::contains (`FillerBudgetService.kt:22-24`)
    // -----------------------------------------------------------------------

    /// C6. `!isBefore(start) && !isAfter(end)` — inclusive at both ends. An
    /// exclusive port passes every interior case and silently drops the two days
    /// that decide which period a boundary worklog belongs to.
    #[test]
    fn contains_includes_both_endpoints_and_excludes_the_days_either_side() {
        let p = period("2026-09-16", "2026-09-30");
        assert!(p.contains(date("2026-09-16")), "the first day is inside");
        assert!(p.contains(date("2026-09-30")), "the last day is inside");
        assert!(p.contains(date("2026-09-23")));
        assert!(!p.contains(date("2026-09-15")), "the day before is outside");
        assert!(!p.contains(date("2026-10-01")), "the day after is outside");
    }

    /// C6. A one-day period contains exactly that day.
    #[test]
    fn a_single_day_period_contains_only_that_day() {
        let p = period("2026-09-16", "2026-09-16");
        assert!(p.contains(date("2026-09-16")));
        assert!(!p.contains(date("2026-09-17")));
    }

    // -----------------------------------------------------------------------
    // billing_periods_in_range (`FillerBudgetService.kt:49-57`)
    // -----------------------------------------------------------------------

    /// C6. A range wholly inside one period collapses to that period once — the
    /// `mutableSetOf` at `:50` is what dedupes the day-by-day walk.
    #[test]
    fn a_range_inside_one_period_yields_that_period_exactly_once() {
        assert_eq!(
            billing_periods_in_range(date("2026-09-02"), date("2026-09-10")),
            vec![period("2026-09-01", "2026-09-15")]
        );
    }

    /// C6. A range sitting exactly on a period's own bounds is still one period.
    #[test]
    fn a_range_matching_a_period_exactly_yields_one_period() {
        assert_eq!(
            billing_periods_in_range(date("2026-09-01"), date("2026-09-15")),
            vec![period("2026-09-01", "2026-09-15")]
        );
        assert_eq!(
            billing_periods_in_range(date("2026-09-16"), date("2026-09-30")),
            vec![period("2026-09-16", "2026-09-30")]
        );
    }

    /// C6. The mid-month boundary, with the range straddling it by one day on each
    /// side. This is the `getBillingPeriodsInRange` across-a-boundary case the test
    /// plan names.
    #[test]
    fn a_range_crossing_the_mid_month_boundary_yields_both_halves_in_order() {
        assert_eq!(
            billing_periods_in_range(date("2026-09-15"), date("2026-09-16")),
            vec![
                period("2026-09-01", "2026-09-15"),
                period("2026-09-16", "2026-09-30"),
            ]
        );
    }

    /// C6. Across a month boundary: the second half of September and the first of
    /// October, and nothing in between.
    #[test]
    fn a_range_crossing_a_month_boundary_yields_the_two_adjacent_periods() {
        assert_eq!(
            billing_periods_in_range(date("2026-09-20"), date("2026-10-05")),
            vec![
                period("2026-09-16", "2026-09-30"),
                period("2026-10-01", "2026-10-15"),
            ]
        );
    }

    /// C6, and the one that separates a correct sort from a plausible wrong one:
    /// `sortedBy { it.start }` orders by the whole date. A sort keyed on the day of
    /// the month, or on the month alone, puts January's periods ahead of December's
    /// and passes every single-year case above.
    #[test]
    fn a_range_crossing_a_year_boundary_stays_in_chronological_order() {
        assert_eq!(
            billing_periods_in_range(date("2026-12-20"), date("2027-01-20")),
            vec![
                period("2026-12-16", "2026-12-31"),
                period("2027-01-01", "2027-01-15"),
                period("2027-01-16", "2027-01-31"),
            ]
        );
    }

    /// C6. The three-period range the plan names as D2's third facet
    /// (`--from 2026-08-10 --to 2026-09-05`).
    #[test]
    fn the_three_period_range_from_the_d2_finding_enumerates_all_three() {
        assert_eq!(
            billing_periods_in_range(date("2026-08-10"), date("2026-09-05")),
            vec![
                period("2026-08-01", "2026-08-15"),
                period("2026-08-16", "2026-08-31"),
                period("2026-09-01", "2026-09-15"),
            ]
        );
    }

    /// C6. `while (!current.isAfter(endDate))` never runs when the range is
    /// inverted, so the result is empty rather than one period or a panic. This is
    /// the case `SettleCommand.kt:452`'s `firstOrNull() ?: getBillingPeriod(from)`
    /// exists to cover.
    #[test]
    fn an_inverted_range_yields_no_periods_at_all() {
        assert!(billing_periods_in_range(date("2026-09-10"), date("2026-09-05")).is_empty());
        assert!(billing_periods_in_range(date("2027-01-01"), date("2026-12-31")).is_empty());
    }

    /// C6. A single-day range is one period — the loop body runs exactly once.
    #[test]
    fn a_single_day_range_yields_the_period_holding_that_day() {
        assert_eq!(
            billing_periods_in_range(date("2026-09-07"), date("2026-09-07")),
            vec![period("2026-09-01", "2026-09-15")]
        );
        assert_eq!(
            billing_periods_in_range(date("2026-09-16"), date("2026-09-16")),
            vec![period("2026-09-16", "2026-09-30")]
        );
    }

    /// C6. A range whose walk crosses February 29th: the leap day must not stall or
    /// skip the walk, and the February second half must carry its 29-day end.
    #[test]
    fn a_range_walking_over_the_leap_day_enumerates_februarys_real_second_half() {
        assert_eq!(
            billing_periods_in_range(date("2024-02-14"), date("2024-03-01")),
            vec![
                period("2024-02-01", "2024-02-15"),
                period("2024-02-16", "2024-02-29"),
                period("2024-03-01", "2024-03-15"),
            ]
        );
    }

    /// C6. A long range produces exactly two periods per month it spans and no
    /// duplicates — the property the `mutableSetOf` guarantees.
    #[test]
    fn a_year_long_range_yields_two_distinct_periods_per_month() {
        let periods = billing_periods_in_range(date("2026-01-01"), date("2026-12-31"));
        assert_eq!(periods.len(), 24);

        let mut sorted = periods.clone();
        sorted.sort_by(|a, b| a.start.cmp(&b.start));
        assert_eq!(periods, sorted, "the result is already in start order");

        for pair in periods.windows(2) {
            assert!(pair[0].start < pair[1].start, "no duplicate periods");
            assert_eq!(
                pair[0].end.succ_opt(),
                Some(pair[1].start),
                "the periods tile the range with no gap: {pair:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // calculate_remaining_budgets (`FillerBudgetService.kt:67-93`)
    // -----------------------------------------------------------------------

    /// C6. `:77` — `filler.maxHoursPerPeriod ?: Double.MAX_VALUE`. G4 says this is
    /// the branch every real run takes today.
    #[test]
    fn a_filler_without_a_period_cap_starts_unlimited() {
        let budgets = calculate_remaining_budgets(
            &[filler("Delivery Practices", "Internal activities", None)],
            &[],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets.get(&key("Delivery Practices", "Internal activities")),
            Some(&UNLIMITED)
        );
    }

    /// C6. `:77-78` — a configured cap is the starting allowance verbatim.
    #[test]
    fn a_configured_cap_is_the_starting_budget() {
        let budgets = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(12.5),
            )],
            &[],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets.get(&key("Delivery Practices", "Internal activities")),
            Some(&12.5)
        );
    }

    /// C6. `:82-89` — worklogs inside the period reduce the allowance.
    #[test]
    fn worklogs_inside_the_period_reduce_the_budget_by_their_logged_hours() {
        let budgets = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(10.0),
            )],
            &[(
                worklog("Delivery Practices", "Internal activities", 2.5),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets.get(&key("Delivery Practices", "Internal activities")),
            Some(&7.5)
        );
    }

    /// C6. `:83` — `if (!billingPeriod.contains(date)) continue`. The two worklogs
    /// here sit one day outside each end and must not be counted; the two on the
    /// boundary days must.
    #[test]
    fn only_worklogs_inside_the_period_count_and_the_boundary_days_are_inside() {
        let p = period("2026-09-16", "2026-09-30");
        let w = |d: &str| {
            (
                worklog("Delivery Practices", "Internal activities", 1.0),
                date(d),
            )
        };

        let outside = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(10.0),
            )],
            &[w("2026-09-15"), w("2026-10-01")],
            &p,
        );
        assert_eq!(
            outside.get(&key("Delivery Practices", "Internal activities")),
            Some(&10.0),
            "neighbouring days must not spend this period's budget"
        );

        let inside = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(10.0),
            )],
            &[w("2026-09-16"), w("2026-09-30")],
            &p,
        );
        assert_eq!(
            inside.get(&key("Delivery Practices", "Internal activities")),
            Some(&8.0),
            "both boundary days are inside the period"
        );
    }

    /// C6. `:87` — the guard is `currentBudget < Double.MAX_VALUE`, so an
    /// uncapped filler is never reduced no matter how many hours match it.
    ///
    /// This case pins the outcome, not the guard: the mutation check found that
    /// deleting the guard leaves this test green, because `Double.MAX_VALUE - 8.0`
    /// **is** `Double.MAX_VALUE` in `f64` — every realistic number of hours is far
    /// below `MAX`'s ULP of about 2^971. The case below pins the guard itself.
    #[test]
    fn an_unlimited_budget_is_never_reduced_by_existing_worklogs() {
        let budgets = calculate_remaining_budgets(
            &[filler("Delivery Practices", "Internal activities", None)],
            &[
                (
                    worklog("Delivery Practices", "Internal activities", 6.0),
                    date("2026-09-04"),
                ),
                (
                    worklog("Delivery Practices", "Internal activities", 8.0),
                    date("2026-09-05"),
                ),
            ],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets.get(&key("Delivery Practices", "Internal activities")),
            Some(&UNLIMITED)
        );
    }

    /// C6, `:87`, and **synthetic by construction** — it says so rather than
    /// pretending to be a regression test for observed behaviour.
    ///
    /// `if (currentBudget != null && currentBudget < Double.MAX_VALUE)` is the only
    /// thing stopping an uncapped filler's sentinel from being decremented, and it
    /// is unobservable at every realistic magnitude: `MAX - 1e290` is still `MAX`,
    /// and only hours above roughly `1e292` move it. So the only input that can
    /// tell the guard from its absence is an absurd one. Without this case, deleting
    /// the guard passes the whole suite — measured, not assumed.
    #[test]
    fn the_unlimited_guard_holds_against_hours_large_enough_to_move_the_sentinel() {
        let budgets = calculate_remaining_budgets(
            &[filler("Delivery Practices", "Internal activities", None)],
            &[(
                worklog("Delivery Practices", "Internal activities", 1.0e300),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            UNLIMITED,
            "the sentinel survives even hours that could move it"
        );
        // The premise of the case, stated so a reader does not have to take the
        // magnitude on trust.
        assert_ne!(UNLIMITED - 1.0e300, UNLIMITED);
        assert_eq!(UNLIMITED - 1.0e290, UNLIMITED);
    }

    /// C6. `:86-87` — `budgets[key]` is null for a worklog matching no filler, so
    /// nothing is written. The map holds only configured fillers.
    #[test]
    fn a_worklog_matching_no_filler_creates_no_budget_entry() {
        let budgets = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(10.0),
            )],
            &[(
                worklog("Velocitor: NLP", "Development work", 4.0),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(budgets.len(), 1);
        assert!(!budgets.contains_key(&key("Velocitor: NLP", "Development work")));
    }

    /// C6. `:88` — `maxOf(0.0, currentBudget - loggedHours)`. Over-consumption
    /// floors at zero; the budget never goes negative on this path. This is the
    /// answer to "what does Kotlin do when the budget goes into the red": it does
    /// not let it.
    #[test]
    fn over_consumption_by_worklogs_floors_the_budget_at_zero_rather_than_going_negative() {
        let budgets = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(4.0),
            )],
            &[(
                worklog("Delivery Practices", "Internal activities", 10.0),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        let remaining = budgets[&key("Delivery Practices", "Internal activities")];
        assert_eq!(remaining, 0.0);
        assert!(
            remaining >= 0.0,
            "the floor is the whole point of maxOf(0.0, …)"
        );
    }

    /// C6. The exactly-spent case, which is the boundary between the two branches
    /// of the floor.
    #[test]
    fn a_budget_spent_to_exactly_zero_lands_on_zero() {
        let budgets = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(4.0),
            )],
            &[(
                worklog("Delivery Practices", "Internal activities", 4.0),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            0.0
        );
    }

    /// C6. `:82` iterates every worklog, so several in one period accumulate
    /// against the same allowance rather than each being measured against the full
    /// cap.
    #[test]
    fn several_worklogs_in_one_period_accumulate_against_the_same_budget() {
        let budgets = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(6.0),
            )],
            &[
                (
                    worklog("Delivery Practices", "Internal activities", 1.5),
                    date("2026-09-02"),
                ),
                (
                    worklog("Delivery Practices", "Internal activities", 1.0),
                    date("2026-09-03"),
                ),
                (
                    worklog("Delivery Practices", "Internal activities", 2.0),
                    date("2026-09-09"),
                ),
            ],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            1.5
        );
    }

    /// C6. Accumulation past the cap still floors at zero rather than carrying a
    /// negative into the next subtraction.
    #[test]
    fn accumulation_past_the_cap_stays_at_zero_across_further_worklogs() {
        let budgets = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(3.0),
            )],
            &[
                (
                    worklog("Delivery Practices", "Internal activities", 2.0),
                    date("2026-09-02"),
                ),
                (
                    worklog("Delivery Practices", "Internal activities", 2.0),
                    date("2026-09-03"),
                ),
                (
                    worklog("Delivery Practices", "Internal activities", 2.0),
                    date("2026-09-04"),
                ),
            ],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            0.0
        );
    }

    /// C6, and the `FillerKey`-as-a-map-key contract: two fillers in the same
    /// DevPro project with different task titles are two budgets, not one. A key
    /// built from the project alone collapses them and makes the second filler
    /// spend the first's allowance.
    #[test]
    fn two_task_titles_in_one_project_keep_separate_budgets() {
        let budgets = calculate_remaining_budgets(
            &[
                filler("Delivery Practices", "Internal activities", Some(10.0)),
                filler("Delivery Practices", "Knowledge sharing", Some(10.0)),
            ],
            &[(
                worklog("Delivery Practices", "Internal activities", 4.0),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(budgets.len(), 2);
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            6.0
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Knowledge sharing")],
            10.0,
            "the other title's budget is untouched"
        );
    }

    /// C6. The mirror image: one task title under two projects. A key built from
    /// the title alone collapses these.
    #[test]
    fn one_task_title_under_two_projects_keeps_separate_budgets() {
        let budgets = calculate_remaining_budgets(
            &[
                filler("Delivery Practices", "Internal activities", Some(10.0)),
                filler("Velocitor: NLP", "Internal activities", Some(10.0)),
            ],
            &[(
                worklog("Velocitor: NLP", "Internal activities", 3.0),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            10.0
        );
        assert_eq!(budgets[&key("Velocitor: NLP", "Internal activities")], 7.0);
    }

    /// C6. `:85` keys the subtraction on `worklog.projectShortName`, not on the
    /// worklog's project id or its billability — a worklog carrying the right title
    /// under the wrong project spends nothing.
    #[test]
    fn the_subtraction_is_keyed_on_project_short_name_and_task_title_together() {
        let budgets = calculate_remaining_budgets(
            &[filler(
                "Delivery Practices",
                "Internal activities",
                Some(10.0),
            )],
            &[
                (
                    worklog("Delivery Practices", "Something else", 4.0),
                    date("2026-09-04"),
                ),
                (
                    worklog("Other Project", "Internal activities", 4.0),
                    date("2026-09-04"),
                ),
            ],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            10.0
        );
    }

    /// C6. `:78` is an unconditional `put`, so two filler configs sharing a key
    /// leave the **last** one's cap in the map. `entry().or_insert()` keeps the
    /// first and would silently reverse it — the same last-wins shape the plan
    /// flags at `BorrowerService.kt:122-123` and `Aggregator.kt:105`.
    #[test]
    fn duplicate_filler_configs_leave_the_last_ones_cap_in_the_map() {
        let budgets = calculate_remaining_budgets(
            &[
                filler("Delivery Practices", "Internal activities", Some(10.0)),
                filler("Delivery Practices", "Internal activities", Some(3.0)),
            ],
            &[],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(budgets.len(), 1);
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            3.0
        );
    }

    /// C6. Same shape, with the cap and the absence of one swapped: a later
    /// uncapped duplicate makes the filler unlimited again.
    #[test]
    fn a_later_uncapped_duplicate_overwrites_an_earlier_cap() {
        let budgets = calculate_remaining_budgets(
            &[
                filler("Delivery Practices", "Internal activities", Some(10.0)),
                filler("Delivery Practices", "Internal activities", None),
            ],
            &[(
                worklog("Delivery Practices", "Internal activities", 4.0),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            UNLIMITED
        );
    }

    /// C6. No fillers configured means no budgets at all, whatever worklogs exist.
    #[test]
    fn no_fillers_means_an_empty_budget_map() {
        let budgets = calculate_remaining_budgets(
            &[],
            &[(
                worklog("Delivery Practices", "Internal activities", 4.0),
                date("2026-09-04"),
            )],
            &period("2026-09-01", "2026-09-15"),
        );
        assert!(budgets.is_empty());
    }

    // -----------------------------------------------------------------------
    // has_remaining_budget (`FillerBudgetService.kt:98-107`)
    // -----------------------------------------------------------------------

    /// C6. `:105` — `budgets[key] ?: Double.MAX_VALUE`. An unconfigured filler is
    /// unlimited, which is how the whole feature stays dead under the live config.
    #[test]
    fn an_unknown_key_is_treated_as_unlimited() {
        let budgets: Budgets = HashMap::new();
        assert!(has_remaining_budget(
            &budgets,
            "Delivery Practices",
            "Internal activities",
            8.0
        ));
    }

    /// C6. `:106` — `remaining >= minRequired`. Exactly on the minimum is
    /// available; a strict `>` would refuse the last quarter-hour of every budget.
    #[test]
    fn a_budget_sitting_exactly_on_the_minimum_is_still_available() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 0.25);
        assert!(has_remaining_budget(
            &budgets,
            "Delivery Practices",
            "Internal activities",
            DEFAULT_MIN_REQUIRED_HOURS
        ));
    }

    /// C6. One tick below the minimum is not available.
    #[test]
    fn a_budget_just_under_the_minimum_is_not_available() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 0.24);
        assert!(!has_remaining_budget(
            &budgets,
            "Delivery Practices",
            "Internal activities",
            DEFAULT_MIN_REQUIRED_HOURS
        ));
    }

    /// C6. A spent budget is not available, and an unlimited one always is.
    #[test]
    fn a_zero_budget_is_unavailable_while_an_unlimited_one_always_is() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 0.0);
        budgets.insert(key("Velocitor: NLP", "Development work"), UNLIMITED);

        assert!(!has_remaining_budget(
            &budgets,
            "Delivery Practices",
            "Internal activities",
            DEFAULT_MIN_REQUIRED_HOURS
        ));
        assert!(has_remaining_budget(
            &budgets,
            "Velocitor: NLP",
            "Development work",
            DEFAULT_MIN_REQUIRED_HOURS
        ));
    }

    /// C6. `:102` — the Kotlin default parameter, which Rust has no syntax for.
    /// The constant is what `FillerService.kt:96` gets by omitting the argument.
    #[test]
    fn the_default_minimum_required_is_a_quarter_hour() {
        assert_eq!(DEFAULT_MIN_REQUIRED_HOURS, 0.25);
    }

    /// C6. The minimum is a parameter, not a constant baked into the comparison.
    #[test]
    fn the_minimum_required_is_honoured_as_given() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 1.0);
        assert!(has_remaining_budget(
            &budgets,
            "Delivery Practices",
            "Internal activities",
            1.0
        ));
        assert!(!has_remaining_budget(
            &budgets,
            "Delivery Practices",
            "Internal activities",
            1.25
        ));
    }

    // -----------------------------------------------------------------------
    // consume_budget (`FillerBudgetService.kt:112-130`)
    // -----------------------------------------------------------------------

    /// C6. `:121-123` — an unlimited budget grants the full request and writes
    /// nothing back, so an absent key stays absent rather than being created with
    /// `MAX_VALUE - hours`.
    #[test]
    fn consuming_an_unlimited_budget_grants_the_full_request_and_stores_nothing() {
        let mut budgets: Budgets = HashMap::new();
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                3.5
            ),
            3.5
        );
        assert!(
            budgets.is_empty(),
            "no entry is created for an unlimited filler"
        );

        budgets.insert(key("Velocitor: NLP", "Development work"), UNLIMITED);
        assert_eq!(
            consume_budget(&mut budgets, "Velocitor: NLP", "Development work", 3.5),
            3.5
        );
        assert_eq!(
            budgets[&key("Velocitor: NLP", "Development work")],
            UNLIMITED
        );
    }

    /// C6. `:126-128` — a request under the allowance is granted in full and
    /// deducted.
    #[test]
    fn consuming_less_than_available_grants_it_all_and_deducts_it() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 5.0);
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                1.5
            ),
            1.5
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            3.5
        );
    }

    /// C6. `:126` — `minOf(requestedHours, available)`. Over-request is granted
    /// only what is left, and the remainder lands on zero rather than going
    /// negative. The returned value, not the request, is what the filler books.
    #[test]
    fn consuming_more_than_available_grants_only_the_remainder_and_leaves_zero() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 1.25);
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                4.0
            ),
            1.25
        );
        let remaining = budgets[&key("Delivery Practices", "Internal activities")];
        assert_eq!(remaining, 0.0);
        assert!(remaining >= 0.0, "consume never drives a budget negative");
    }

    /// C6. The exact-fit case between the two branches of `minOf`.
    #[test]
    fn consuming_exactly_the_available_amount_leaves_zero() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 2.0);
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                2.0
            ),
            2.0
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            0.0
        );
    }

    /// C6. A spent budget grants nothing and stays spent.
    #[test]
    fn consuming_a_spent_budget_grants_nothing() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 0.0);
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                2.0
            ),
            0.0
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            0.0
        );
    }

    /// C6. Repeated consumption walks one allowance down across several days,
    /// which is what makes the map `Mutable` in the Kotlin signature.
    #[test]
    fn repeated_consumption_walks_one_budget_down_to_zero() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 3.0);

        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                1.0
            ),
            1.0
        );
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                1.5
            ),
            1.5
        );
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                1.0
            ),
            0.5
        );
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                1.0
            ),
            0.0
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            0.0
        );
    }

    /// C6. `:121`'s `available >= Double.MAX_VALUE - 1` is a sentinel test, not a
    /// "this budget is large" test, and the `- 1.0` is a no-op — one is far below
    /// `MAX`'s ULP, so the comparison is exactly "is this the sentinel".
    ///
    /// The plausible wrong port is `available.is_infinite()`, which reads like the
    /// same idea and is not: `Double.MAX_VALUE` is finite, so under it the sentinel
    /// would start being deducted from and every uncapped filler would quietly gain
    /// a budget. A large finite budget going the other way — deducted, not waved
    /// through — is the half of the contract that fixes the direction.
    ///
    /// The magnitude is chosen rather than picked: `1e300 - 2.0` **is** `1e300` in
    /// `f64`, so a budget that big would make the second assertion vacuous. `1e15`
    /// has a ULP of about an eighth and registers the subtraction.
    #[test]
    fn the_unlimited_test_is_a_sentinel_test_not_a_size_test() {
        assert_eq!(
            UNLIMITED - 1.0,
            UNLIMITED,
            "the -1 cannot move an f64 of this size"
        );
        assert!(UNLIMITED.is_finite(), "the sentinel is MAX, not infinity");

        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 1.0e15);
        assert_eq!(
            consume_budget(
                &mut budgets,
                "Delivery Practices",
                "Internal activities",
                2.0
            ),
            2.0
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            1.0e15 - 2.0,
            "a large but finite budget is deducted from, not waved through"
        );
    }

    /// C6. `consumeBudget` reaches only the key it is given; the neighbouring
    /// filler's allowance is untouched.
    #[test]
    fn consuming_one_filler_does_not_touch_another() {
        let mut budgets: Budgets = HashMap::new();
        budgets.insert(key("Delivery Practices", "Internal activities"), 4.0);
        budgets.insert(key("Delivery Practices", "Knowledge sharing"), 4.0);

        consume_budget(
            &mut budgets,
            "Delivery Practices",
            "Internal activities",
            2.0,
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Internal activities")],
            2.0
        );
        assert_eq!(
            budgets[&key("Delivery Practices", "Knowledge sharing")],
            4.0
        );
    }

    // -----------------------------------------------------------------------
    // PeriodBudgets — D2's third facet
    // -----------------------------------------------------------------------

    /// D2, third facet. The regression test for the defect at
    /// `SettleCommand.kt:449-456`: the incumbent enumerates every period the range
    /// spans and then budgets the whole range against `billingPeriods.first()`.
    ///
    /// Here the first period already holds 3 of a 4-hour allowance, and the two
    /// later periods hold nothing. Under the incumbent every day in the range sees
    /// 1.0 remaining. Under this port each period carries its own allowance.
    #[test]
    fn each_period_carries_its_own_allowance_instead_of_the_first_periods() {
        let fillers = [filler(
            "Delivery Practices",
            "Internal activities",
            Some(4.0),
        )];
        let worklogs = [(
            worklog("Delivery Practices", "Internal activities", 3.0),
            date("2026-08-12"),
        )];

        let budgets = PeriodBudgets::calculate_if_configured(
            &fillers,
            &worklogs,
            date("2026-08-10"),
            date("2026-09-05"),
        )
        .expect("a filler carries a period cap, so budgets exist");

        let k = key("Delivery Practices", "Internal activities");
        assert_eq!(
            budgets.for_date(date("2026-08-12")).map(|b| b[&k]),
            Some(1.0),
            "the period that holds the worklog is reduced by it"
        );
        assert_eq!(
            budgets.for_date(date("2026-08-20")).map(|b| b[&k]),
            Some(4.0),
            "the second half of August was never charged for the first half's hours"
        );
        assert_eq!(
            budgets.for_date(date("2026-09-03")).map(|b| b[&k]),
            Some(4.0),
            "September likewise"
        );
    }

    /// D2, third facet. The periods are exactly those the range spans, in order.
    #[test]
    fn the_budget_set_covers_every_period_the_range_spans() {
        let fillers = [filler(
            "Delivery Practices",
            "Internal activities",
            Some(4.0),
        )];
        let budgets = PeriodBudgets::calculate_if_configured(
            &fillers,
            &[],
            date("2026-08-10"),
            date("2026-09-05"),
        )
        .expect("budgets exist");

        assert_eq!(
            budgets.periods().cloned().collect::<Vec<_>>(),
            vec![
                period("2026-08-01", "2026-08-15"),
                period("2026-08-16", "2026-08-31"),
                period("2026-09-01", "2026-09-15"),
            ]
        );
    }

    /// D2, third facet. `for_date` resolves through `contains`, so every day of a
    /// period reaches the same map and the boundary days reach the right one.
    #[test]
    fn a_date_resolves_to_the_budgets_of_the_period_holding_it() {
        let fillers = [filler(
            "Delivery Practices",
            "Internal activities",
            Some(4.0),
        )];
        let worklogs = [(
            worklog("Delivery Practices", "Internal activities", 1.0),
            date("2026-09-16"),
        )];
        let budgets = PeriodBudgets::calculate_if_configured(
            &fillers,
            &worklogs,
            date("2026-09-01"),
            date("2026-09-30"),
        )
        .expect("budgets exist");

        let k = key("Delivery Practices", "Internal activities");
        assert_eq!(
            budgets.for_date(date("2026-09-01")).map(|b| b[&k]),
            Some(4.0)
        );
        assert_eq!(
            budgets.for_date(date("2026-09-15")).map(|b| b[&k]),
            Some(4.0)
        );
        assert_eq!(
            budgets.for_date(date("2026-09-16")).map(|b| b[&k]),
            Some(3.0)
        );
        assert_eq!(
            budgets.for_date(date("2026-09-30")).map(|b| b[&k]),
            Some(3.0)
        );
    }

    /// D2, third facet. A date outside every period has no budget map, which every
    /// reader here already treats as unlimited — the same answer the incumbent's
    /// `periodBudgets = null` branch gives.
    #[test]
    fn a_date_outside_every_period_has_no_budget_map() {
        let fillers = [filler(
            "Delivery Practices",
            "Internal activities",
            Some(4.0),
        )];
        let budgets = PeriodBudgets::calculate_if_configured(
            &fillers,
            &[],
            date("2026-09-01"),
            date("2026-09-15"),
        )
        .expect("budgets exist");

        assert!(budgets.for_date(date("2026-08-31")).is_none());
        assert!(budgets.for_date(date("2026-09-16")).is_none());
    }

    /// D2, third facet, and G4: `config.fillers.any { it.maxHoursPerPeriod != null }`
    /// is false on every real run today, so this is the branch the live config
    /// takes and the reason the whole feature is dead code.
    #[test]
    fn no_filler_with_a_period_cap_means_no_budgets_at_all() {
        let fillers = [
            filler("Delivery Practices", "Internal activities", None),
            filler("Velocitor: NLP", "Development work", None),
        ];
        assert!(
            PeriodBudgets::calculate_if_configured(
                &fillers,
                &[],
                date("2026-09-01"),
                date("2026-09-15")
            )
            .is_none()
        );
        assert!(
            PeriodBudgets::calculate_if_configured(
                &[],
                &[],
                date("2026-09-01"),
                date("2026-09-15")
            )
            .is_none()
        );
    }

    /// D2, third facet. `:450`'s `any` fires on one capped filler among many, and
    /// the uncapped ones still get their unlimited entries in each period's map.
    #[test]
    fn one_capped_filler_among_uncapped_ones_still_builds_the_budget_set() {
        let fillers = [
            filler("Delivery Practices", "Internal activities", None),
            filler("Velocitor: NLP", "Development work", Some(6.0)),
        ];
        let budgets = PeriodBudgets::calculate_if_configured(
            &fillers,
            &[],
            date("2026-09-01"),
            date("2026-09-15"),
        )
        .expect("one capped filler is enough");

        let day = budgets
            .for_date(date("2026-09-04"))
            .expect("the day is in range");
        assert_eq!(
            day[&key("Delivery Practices", "Internal activities")],
            UNLIMITED
        );
        assert_eq!(day[&key("Velocitor: NLP", "Development work")], 6.0);
    }

    /// D2, third facet. `:452`'s `firstOrNull() ?: getBillingPeriod(from)` — an
    /// inverted range enumerates no periods, and the fallback puts `from`'s own
    /// period in rather than leaving the caller with an empty set.
    #[test]
    fn an_inverted_range_falls_back_to_the_period_holding_from() {
        let fillers = [filler(
            "Delivery Practices",
            "Internal activities",
            Some(4.0),
        )];
        let budgets = PeriodBudgets::calculate_if_configured(
            &fillers,
            &[],
            date("2026-09-20"),
            date("2026-09-05"),
        )
        .expect("budgets exist");

        assert_eq!(
            budgets.periods().cloned().collect::<Vec<_>>(),
            vec![period("2026-09-16", "2026-09-30")]
        );
    }

    /// D2, third facet. Consuming through one period's map leaves the other
    /// periods' allowances untouched — the property the single-map incumbent
    /// cannot have.
    #[test]
    fn consuming_in_one_period_leaves_the_other_periods_untouched() {
        let fillers = [filler(
            "Delivery Practices",
            "Internal activities",
            Some(4.0),
        )];
        let mut budgets = PeriodBudgets::calculate_if_configured(
            &fillers,
            &[],
            date("2026-09-01"),
            date("2026-09-30"),
        )
        .expect("budgets exist");

        let first = budgets
            .for_date_mut(date("2026-09-04"))
            .expect("the day is in range");
        assert_eq!(
            consume_budget(first, "Delivery Practices", "Internal activities", 2.5),
            2.5
        );

        let k = key("Delivery Practices", "Internal activities");
        assert_eq!(
            budgets.for_date(date("2026-09-04")).map(|b| b[&k]),
            Some(1.5)
        );
        assert_eq!(
            budgets.for_date(date("2026-09-20")).map(|b| b[&k]),
            Some(4.0),
            "the second half still holds its full allowance"
        );
    }

    /// D2, third facet. `calculate` takes the periods it is given, so a range
    /// inside one period yields exactly one map and behaves like the incumbent —
    /// which is why the fix is invisible on every single-period run.
    #[test]
    fn a_range_inside_one_period_yields_the_same_single_map_the_incumbent_built() {
        let fillers = [filler(
            "Delivery Practices",
            "Internal activities",
            Some(4.0),
        )];
        let worklogs = [(
            worklog("Delivery Practices", "Internal activities", 1.0),
            date("2026-09-02"),
        )];
        let budgets = PeriodBudgets::calculate_if_configured(
            &fillers,
            &worklogs,
            date("2026-09-02"),
            date("2026-09-10"),
        )
        .expect("budgets exist");

        assert_eq!(budgets.periods().count(), 1);
        assert_eq!(
            budgets
                .for_date(date("2026-09-10"))
                .map(|b| b[&key("Delivery Practices", "Internal activities")]),
            Some(3.0)
        );
    }

    /// D2, third facet. `calculate` is the primitive under `calculate_if_configured`
    /// and honours a hand-made period set, including one that leaves gaps.
    #[test]
    fn calculate_honours_the_period_set_it_is_given() {
        let fillers = [filler(
            "Delivery Practices",
            "Internal activities",
            Some(4.0),
        )];
        let worklogs = [(
            worklog("Delivery Practices", "Internal activities", 1.0),
            date("2026-09-20"),
        )];
        let budgets = PeriodBudgets::calculate(
            &fillers,
            &worklogs,
            &[
                period("2026-09-16", "2026-09-30"),
                period("2026-11-01", "2026-11-15"),
            ],
        );

        let k = key("Delivery Practices", "Internal activities");
        assert_eq!(
            budgets.for_date(date("2026-09-20")).map(|b| b[&k]),
            Some(3.0)
        );
        assert_eq!(
            budgets.for_date(date("2026-11-02")).map(|b| b[&k]),
            Some(4.0)
        );
        assert!(
            budgets.for_date(date("2026-10-10")).is_none(),
            "the gap between the two given periods has no budgets"
        );
    }

    /// C28's cheap guard, applied here: the same input built twice gives the same
    /// answer. The budget map is keyed and never iterated, so a randomized
    /// `HashMap` cannot leak out — this test is what says so rather than the
    /// comment at the top of the file.
    #[test]
    fn the_same_input_produces_the_same_budgets_twice() {
        let fillers = [
            filler("Delivery Practices", "Internal activities", Some(4.0)),
            filler("Delivery Practices", "Knowledge sharing", Some(2.0)),
            filler("Velocitor: NLP", "Development work", None),
        ];
        let worklogs = [
            (
                worklog("Delivery Practices", "Internal activities", 1.5),
                date("2026-09-02"),
            ),
            (
                worklog("Delivery Practices", "Knowledge sharing", 0.5),
                date("2026-09-20"),
            ),
        ];

        let first = PeriodBudgets::calculate_if_configured(
            &fillers,
            &worklogs,
            date("2026-09-01"),
            date("2026-09-30"),
        );
        let second = PeriodBudgets::calculate_if_configured(
            &fillers,
            &worklogs,
            date("2026-09-01"),
            date("2026-09-30"),
        );
        assert_eq!(first, second);
    }
}
