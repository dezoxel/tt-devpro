//! Filler synthesis on meeting-only days — the port of `service/FillerService.kt`.
//!
//! A day where *every* aggregate is a meeting cannot be scaled to 8h, because
//! meetings keep their real hours (C5). The incumbent closes the gap by inventing
//! work entries drawn from `fillers:` in the config, restricted to projects that
//! already appear in the day, one per project, capped at `max_synthetic_hours` and
//! at each filler's period budget (C6).
//!
//! Three things about this file are worth reading before changing it.
//!
//! **The randomness is injected, and that is the only structural change.** G2 says
//! filler selection is random and therefore not reproducible, which is why this
//! module has no Kotlin test at all. `FillerService.kt:107` (`availableFillers
//! .random()`) and `FillerService.kt:133` (`Random.nextDouble(minAllowed, maxAllowed)`) both go
//! through [`FillerRandom`]. The production implementation [`ThreadFillerRandom`]
//! is `rand`'s thread RNG behind Kotlin's own `nextDouble(from, until)` algorithm,
//! so the shipped behaviour is what it was; the tests get a stub and C6 becomes
//! checkable for the first time.
//!
//! **The budget argument is `PeriodBudgets`, which is D2's third facet.** The
//! incumbent is handed one budget map built from the *first* billing period of the
//! settle range (`SettleCommand.kt:449-456`) and applies it to every day. This port
//! asks `filler_budget` for the period of the day being filled. Everything inside
//! the map — the key, the unlimited sentinel, the `>=` comparisons — is verbatim.
//!
//! **The loop terminates on the `usedProjects` set, not on the hours.** Two of the
//! four ways an iteration can end (`continue` at `FillerService.kt:125-127`, and a consumption that
//! adds no entry) leave `hoursToFill` untouched. What bounds the loop is that a
//! filler is marked used at `FillerService.kt:108`, *before* either of those exits, and the
//! candidate filter at `FillerService.kt:94` excludes used projects — so every non-breaking
//! iteration removes at least one project from the candidate set. Moving the
//! `usedProjects.add` below the `continue` would turn a filler with
//! `max_hours < 0.25` into an infinite loop.

use chrono::NaiveDate;

use crate::config::Filler;
use crate::fmt::java_dbl;
use crate::model::{FillerEntry, FillerKey, NormalizedAggregate};
use crate::service::filler_budget::{
    Budgets, DEFAULT_MIN_REQUIRED_HOURS, PeriodBudgets, UNLIMITED, consume_budget,
    has_remaining_budget,
};
use crate::service::normalizer::{HOUR_INCREMENT, java_min, round_to_quarter};

/// `FillerService.kt:15`. Declared again here rather than shared with
/// `normalizer.rs`, because Kotlin declares it twice too and the two are not the
/// same knob — `TimeNormalizer`'s is the scaling target, this one is the gap.
const TARGET_HOURS: f64 = 8.0;

/// The two random draws `FillerService` makes, behind one trait so tests can pin
/// them (the "randomness becomes injectable" note under the plan's `## Module map`).
///
/// Nothing here is a general-purpose RNG interface: the two methods are exactly the
/// two calls the incumbent makes, with the same contracts.
pub trait FillerRandom {
    /// `availableFillers.random()` (`FillerService.kt:107`), which is
    /// `Random.Default.nextInt(size)` on a list Kotlin has already checked is
    /// non-empty.
    ///
    /// `len` is always `>= 1`; the return value must be `< len`.
    fn choose_index(&mut self, len: usize) -> usize;

    /// `Random.nextDouble(from, until)` (`FillerService.kt:133`), a draw in
    /// `[from, until)`.
    ///
    /// The call site guarantees `until > from` — it is the `else` arm of
    /// `if (maxAllowed <= minAllowed)` — so an implementation may treat an empty
    /// range as a programming error, which is what Kotlin's own `require` does.
    fn next_double_in(&mut self, from: f64, until: f64) -> f64;
}

/// The production source: `rand`'s thread-local RNG, driving Kotlin's own
/// `nextDouble(from, until)` arithmetic.
///
/// Using `rand`'s `random_range(from..until)` directly would be *a* uniform draw
/// but not *this* one; the difference is unobservable by construction (G2 says the
/// output is unrepeatable either way), and going through
/// [`kotlin_next_double_in`] costs one multiply and removes the question.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadFillerRandom;

impl FillerRandom for ThreadFillerRandom {
    fn choose_index(&mut self, len: usize) -> usize {
        rand::Rng::random_range(&mut rand::rng(), 0..len)
    }

    fn next_double_in(&mut self, from: f64, until: f64) -> f64 {
        let unit: f64 = rand::Rng::random(&mut rand::rng());
        kotlin_next_double_in(from, until, unit)
    }
}

/// `kotlin.random.Random.nextDouble(from, until)` with the underlying
/// `nextDouble()` draw passed in as `unit`, so the algorithm itself is testable.
///
/// Transcribed from `kotlin-stdlib` 1.9.22 `Random.nextDouble(Double, Double)`:
/// a `require(until > from)`, an infinite-`size` branch that halves both ends to
/// avoid overflowing, and a final step-down of one ULP when rounding pushes the
/// result onto the exclusive bound.
///
/// **Transcribed from the bytecode, not from memory.** The stdlib jar was taken out
/// of the Gradle cache and `javap -p -c` run over `kotlin/random/Random` and
/// `kotlin/random/RandomKt` under JDK 21, so each branch below is one of those
/// instructions: `checkRangeBounds` at offset 2; the halving branch at 10..69,
/// entered only when `size` is infinite *and* `isInfinite`/`isNaN` are both false
/// on each end separately; `from + r1 + r1` at 72..95; `from + unit * size` at
/// 98..106; the bound test at 109..128.
///
/// That bound test is `dcmpl` followed by `iflt`, which returns the raw `r` exactly
/// when `r < until`. `dcmpl` yields `-1` for NaN, so a NaN `r` is returned as-is
/// rather than stepped down — and `r >= until` below agrees, because `NaN >= x` is
/// false. NaN is reachable here: `from = -f64::INFINITY` passes the bounds check
/// and then `-∞ + unit * ∞` is NaN.
///
/// What is **not** measured: no differential run against a live JVM was made for
/// this function as a whole. `Math.nextDown` was (below), the rest is
/// instruction-by-instruction reading.
///
/// That last step is Kotlin's `until.nextDown()`, and it is **not**
/// `f64::from_bits(until.to_bits() - 1)`. The bit trick walks the wrong way for a
/// negative `until`, and at `±0.0` the subtraction underflows the bit pattern to
/// `u64::MAX`, which is a NaN. Measured against `Math.nextDown` on JDK 21
/// (`~/.cache/tt-devpro-rewrite/measurements/nextdown/`, re-run 2026-09-22):
/// they agree on `0.5`, `0.25`, `1.0` and
/// `f64::MIN_POSITIVE_SUBNORMAL`, and diverge on `0.0`, `-0.0`, `-0.25` and `-1.0`.
/// `f64::next_down()` is the faithful counterpart and is available on the declared
/// MSRV of 1.87 (stabilised in 1.86), so there is nothing to trade off.
///
/// `fill_gap` cannot reach a non-positive `until` — `FillerService.kt:125` skips the
/// filler when `maxAllowed < HOUR_INCREMENT`, so the bound is always at least 0.25.
/// This function is `pub` and its callers are not, so it is correct on its own terms
/// rather than on its one caller's.
///
/// # Panics
///
/// When `until <= from`, or when either is NaN — Kotlin's `require` fails on both,
/// since `NaN > NaN` is false. `from == until` panics for the same reason: Kotlin
/// tests `until > from`, not `>=`.
///
/// The message is assembled in `RandomKt.boundsErrorMessage` from three constant-pool
/// strings — `"Random range is empty: ["`, `", "`, `")."` — read out of the class file
/// rather than recalled, which is why the bounds go through [`java_dbl`]: Rust's `{}`
/// renders `1.0` as `1`, Java's `Double.toString` as `1.0`.
pub fn kotlin_next_double_in(from: f64, until: f64, unit: f64) -> f64 {
    assert!(
        until > from,
        "Random range is empty: [{}, {}).",
        java_dbl(from),
        java_dbl(until)
    );

    let size = until - from;
    let r = if size.is_infinite() && from.is_finite() && until.is_finite() {
        let r1 = unit * (until / 2.0 - from / 2.0);
        from + r1 + r1
    } else {
        from + unit * size
    };

    if r >= until { until.next_down() } else { r }
}

/// `FillerService.kt:32-46` (`generateFillers`).
///
/// Days are grouped in first-encounter order (C28, `FillerService.kt:41` — `groupBy` returns a
/// `LinkedHashMap`) and flat-mapped in that order, so the returned list's order is
/// the input's order of dates, and within a date the order fillers were chosen.
///
/// `period_budgets` is [`None`] when no filler configures `max_hours_per_period`,
/// which G4 says is every real run today; it then behaves exactly as the
/// incumbent's `null` map — every filler unlimited. A date falling outside every
/// known period is treated the same way, which is what
/// [`PeriodBudgets::for_date_mut`] already returns `None` for.
pub fn generate_fillers<R: FillerRandom + ?Sized>(
    normalized: &[NormalizedAggregate],
    fillers: &[Filler],
    max_synthetic_hours: f64,
    mut period_budgets: Option<&mut PeriodBudgets>,
    rng: &mut R,
) -> Vec<FillerEntry> {
    // `FillerService.kt:38`.
    if fillers.is_empty() {
        return Vec::new();
    }

    // `FillerService.kt:41` — `normalized.groupBy { it.original.date }`.
    let by_date = group_by_date_in_encounter_order(normalized);

    // `FillerService.kt:43-45` — `flatMap`, so days concatenate in the grouping's order.
    let mut result = Vec::new();
    for (date, day_entries) in by_date {
        let budgets = period_budgets
            .as_deref_mut()
            .and_then(|pb| pb.for_date_mut(date));
        result.extend(generate_fillers_for_day(
            date,
            &day_entries,
            fillers,
            max_synthetic_hours,
            budgets,
            rng,
        ));
    }
    result
}

/// Kotlin's `groupBy`: keys in first-encounter order, values in input order within
/// a group (C28). A `HashMap` would randomize the day order per process and a
/// `BTreeMap` would sort the dates, and neither is what `FillerService.kt:41` does.
fn group_by_date_in_encounter_order(
    normalized: &[NormalizedAggregate],
) -> Vec<(NaiveDate, Vec<&NormalizedAggregate>)> {
    let mut groups: Vec<(NaiveDate, Vec<&NormalizedAggregate>)> = Vec::new();
    for entry in normalized {
        match groups
            .iter_mut()
            .find(|(date, _)| *date == entry.original.date)
        {
            Some((_, bucket)) => bucket.push(entry),
            None => groups.push((entry.original.date, vec![entry])),
        }
    }
    groups
}

/// `FillerService.kt:48-73` (`generateFillersForDay`).
fn generate_fillers_for_day<R: FillerRandom + ?Sized>(
    date: NaiveDate,
    day_entries: &[&NormalizedAggregate],
    fillers: &[Filler],
    max_synthetic_hours: f64,
    budgets: Option<&mut Budgets>,
    rng: &mut R,
) -> Vec<FillerEntry> {
    // `FillerService.kt:56-57` — the gate. One scalable entry anywhere in the day and the
    // normalizer can reach 8h on its own, so nothing is synthesized.
    let all_meetings = day_entries.iter().all(|e| e.is_meeting);
    if !all_meetings {
        return Vec::new();
    }

    // `FillerService.kt:60-61`. Summed left to right, as `sumOf` is: `f64` addition is not
    // associative and the order is part of the answer.
    let current_hours = day_entries
        .iter()
        .fold(0.0_f64, |acc, e| acc + e.normalized_hours);
    let remaining_hours = TARGET_HOURS - current_hours;

    // `FillerService.kt:63`.
    if remaining_hours <= 0.0 {
        return Vec::new();
    }

    // `FillerService.kt:66` — leave room for the borrower, which spends the other half of the same
    // synthetic budget.
    let capped_remaining_hours = java_min(remaining_hours, max_synthetic_hours);

    // `FillerService.kt:69`. A `toSet()` in Kotlin, but only ever asked `contains`
    // (`FillerService.kt:93`), so the dedupe and the insertion order are both
    // unobservable — a `Vec` answers the same question without inviting a hash-order
    // question that does not exist here.
    let present_projects: Vec<&str> = day_entries
        .iter()
        .map(|e| e.original.devpro_project_name.as_str())
        .collect();

    fill_gap(
        date,
        capped_remaining_hours,
        fillers,
        &present_projects,
        budgets,
        rng,
    )
}

/// `FillerService.kt:75-162` (`fillGap`).
fn fill_gap<R: FillerRandom + ?Sized>(
    date: NaiveDate,
    remaining_hours: f64,
    fillers: &[Filler],
    present_projects: &[&str],
    mut budgets: Option<&mut Budgets>,
    rng: &mut R,
) -> Vec<FillerEntry> {
    let mut result: Vec<FillerEntry> = Vec::new();
    let mut hours_to_fill = remaining_hours;
    // `FillerService.kt:84` — read only through `contains`/`add`, like `presentProjects`.
    let mut used_projects: Vec<&str> = Vec::new();

    // `FillerService.kt:87`. The first conjunct is redundant — `HOUR_INCREMENT` is positive, so
    // `>= 0.25` already implies `> 0`, and NaN fails both — and is ported because it
    // is there. `expect` rather than `allow`: if a later edit makes the conjunct
    // meaningful, the attribute itself fails the build instead of going stale.
    #[expect(clippy::redundant_comparisons)]
    while hours_to_fill > 0.0 && hours_to_fill >= HOUR_INCREMENT {
        // `FillerService.kt:92-100`.
        let available_fillers: Vec<&Filler> = fillers
            .iter()
            .filter(|filler| {
                let is_present = present_projects.contains(&filler.devpro_project.as_str());
                let not_used_today = !used_projects.contains(&filler.devpro_project.as_str());
                // `FillerService.kt:96` — the default `minRequired` of the Kotlin signature.
                let has_budget = match budgets.as_deref() {
                    Some(b) => has_remaining_budget(
                        b,
                        &filler.devpro_project,
                        &filler.task_title,
                        DEFAULT_MIN_REQUIRED_HOURS,
                    ),
                    None => true,
                };
                is_present && not_used_today && has_budget
            })
            .collect();

        // `FillerService.kt:102-105`.
        if available_fillers.is_empty() {
            break;
        }

        // `FillerService.kt:107-108`. The `add` happens here, above every later exit — see the
        // termination note at the top of the file.
        let filler = available_fillers[rng.choose_index(available_fillers.len())];
        used_projects.push(filler.devpro_project.as_str());

        // `FillerService.kt:111`.
        let mut max_allowed = java_min(filler.max_hours, hours_to_fill);

        // `FillerService.kt:114-120`. A key with no entry reads as the unlimited sentinel and the
        // `< Double.MAX_VALUE` guard then skips the cap, so a present-but-silent map
        // and an absent map agree.
        if let Some(b) = budgets.as_deref() {
            let key = FillerKey {
                devpro_project: filler.devpro_project.clone(),
                task_title: filler.task_title.clone(),
            };
            let remaining_budget = b.get(&key).copied().unwrap_or(UNLIMITED);
            if remaining_budget < UNLIMITED {
                max_allowed = java_min(max_allowed, remaining_budget);
            }
        }

        // `FillerService.kt:122`. Note the direction: a configured minimum *above* the ceiling is
        // pulled down to the ceiling, never the other way round.
        let min_allowed = java_min(filler.min_hours, max_allowed);

        // `FillerService.kt:125-127` — `continue`, not `break`. The project has already been marked
        // used, so the next iteration sees a strictly smaller candidate set.
        if max_allowed < HOUR_INCREMENT {
            continue;
        }

        // `FillerService.kt:130-134`.
        let raw_hours = if max_allowed <= min_allowed {
            min_allowed
        } else {
            rng.next_double_in(min_allowed, max_allowed)
        };

        // `FillerService.kt:137` — C27's *rounding* quantizer, the one where `0.375` becomes `0.5`.
        // The interactive edit path truncates the same value to `0.25`; both are
        // contracts and unifying them changes hours on real days.
        let hours = round_to_quarter(raw_hours);

        if hours > 0.0 {
            // `FillerService.kt:141-143`.
            let consumed = match budgets.as_deref_mut() {
                Some(b) => consume_budget(b, &filler.devpro_project, &filler.task_title, hours),
                None => hours,
            };

            // `FillerService.kt:145`. Always true when reached — see the module's test
            // `consumption_below_the_increment_is_unreachable_by_construction`.
            if consumed >= HOUR_INCREMENT {
                result.push(FillerEntry {
                    date,
                    devpro_project_name: filler.devpro_project.clone(),
                    task_title: filler.task_title.clone(),
                    billability: filler.billability.clone(),
                    hours: round_to_quarter(consumed),
                });
                hours_to_fill -= round_to_quarter(consumed);
            }
        } else {
            // `FillerService.kt:156-158` — `break`, not `continue`. A draw that rounds to zero ends
            // the day's synthesis entirely, even though other projects are still
            // available.
            break;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DayProjectAggregate;
    use crate::service::filler_budget::BillingPeriod;
    use std::collections::HashMap;

    // ---------------------------------------------------------------- fixtures

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, d).unwrap()
    }

    fn agg(date: NaiveDate, project: &str, hours: f64, is_meeting: bool) -> NormalizedAggregate {
        NormalizedAggregate {
            original: DayProjectAggregate {
                date,
                chrono_project: format!("{project} - DevPro - Work"),
                total_hours: hours,
                descriptions: vec!["Some entry".to_string()],
                devpro_project_name: project.to_string(),
                billability: "Billable".to_string(),
                max_hours: None,
            },
            normalized_hours: hours,
            is_meeting,
        }
    }

    fn filler(project: &str, title: &str, min_hours: f64, max_hours: f64) -> Filler {
        Filler {
            devpro_project: project.to_string(),
            task_title: title.to_string(),
            billability: "NonBillable".to_string(),
            min_hours,
            max_hours,
            max_hours_per_period: None,
        }
    }

    /// The largest `f64` strictly below `1.0` — the top of `nextDouble()`'s range.
    const NEAR_ONE: f64 = 0.999_999_999_999_999_9;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Pick {
        First,
        Last,
    }

    /// A deterministic [`FillerRandom`] that also records what it was asked, so a
    /// test can assert the *candidate set* the incumbent offered the RNG and not
    /// merely the entry that came out.
    ///
    /// Draws route through [`kotlin_next_double_in`], so the tests exercise the
    /// production arithmetic rather than a second, friendlier one.
    struct ScriptedRandom {
        pick: Pick,
        fraction: f64,
        offered_lens: Vec<usize>,
        draw_ranges: Vec<(f64, f64)>,
    }

    impl ScriptedRandom {
        fn new(pick: Pick, fraction: f64) -> Self {
            Self {
                pick,
                fraction,
                offered_lens: Vec::new(),
                draw_ranges: Vec::new(),
            }
        }

        fn first_low() -> Self {
            Self::new(Pick::First, 0.0)
        }

        fn first_high() -> Self {
            Self::new(Pick::First, NEAR_ONE)
        }

        fn last_high() -> Self {
            Self::new(Pick::Last, NEAR_ONE)
        }
    }

    impl FillerRandom for ScriptedRandom {
        fn choose_index(&mut self, len: usize) -> usize {
            self.offered_lens.push(len);
            match self.pick {
                Pick::First => 0,
                Pick::Last => len - 1,
            }
        }

        fn next_double_in(&mut self, from: f64, until: f64) -> f64 {
            self.draw_ranges.push((from, until));
            kotlin_next_double_in(from, until, self.fraction)
        }
    }

    /// Takes the first candidate and blows up if asked for a draw. Used to prove
    /// the `maxAllowed <= minAllowed` arm short-circuits rather than drawing from a
    /// degenerate range.
    struct NoDrawRandom;

    impl FillerRandom for NoDrawRandom {
        fn choose_index(&mut self, _len: usize) -> usize {
            0
        }

        fn next_double_in(&mut self, from: f64, until: f64) -> f64 {
            panic!("next_double_in must not be called; range was [{from}, {until})");
        }
    }

    fn budgets_for(date: NaiveDate, entries: &[(&str, &str, f64)]) -> PeriodBudgets {
        let mut map: Budgets = HashMap::new();
        for (project, title, remaining) in entries {
            map.insert(
                FillerKey {
                    devpro_project: (*project).to_string(),
                    task_title: (*title).to_string(),
                },
                *remaining,
            );
        }
        period_budgets_from(
            BillingPeriod {
                start: date,
                end: date,
            },
            map,
        )
    }

    /// `PeriodBudgets` has no public constructor from a ready-made map, so the test
    /// builds one through `calculate` with a filler carrying the wanted cap and no
    /// worklogs against it. Documented rather than hidden: it is the only way in
    /// from outside the module, and it is exact — `calculate_remaining_budgets`
    /// with an empty worklog list is the identity on the configured caps.
    fn period_budgets_from(period: BillingPeriod, map: Budgets) -> PeriodBudgets {
        let fillers: Vec<Filler> = map
            .iter()
            .map(|(key, remaining)| Filler {
                devpro_project: key.devpro_project.clone(),
                task_title: key.task_title.clone(),
                billability: "NonBillable".to_string(),
                min_hours: 0.0,
                max_hours: 0.0,
                max_hours_per_period: Some(*remaining),
            })
            .collect();
        PeriodBudgets::calculate(&fillers, &[], &[period])
    }

    // ------------------------------------------------- the meeting-only gate, C6

    /// C6, `FillerService.kt:38`. An empty `fillers:` block short-circuits
    /// before any grouping, so a day that would otherwise qualify gets nothing.
    #[test]
    fn an_empty_filler_config_synthesizes_nothing_even_on_a_qualifying_day() {
        let normalized = vec![agg(day(18), "Delivery Practices", 2.0, true)];
        let out = generate_fillers(
            &normalized,
            &[],
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out, vec![]);
    }

    /// C6, `FillerService.kt:56-57`. The gate is `all`, not "mostly" — one scalable
    /// entry in the day and the normalizer can reach 8h by itself.
    #[test]
    fn a_single_non_meeting_entry_disqualifies_the_whole_day() {
        let normalized = vec![
            agg(day(18), "Delivery Practices", 2.0, true),
            agg(day(18), "Delivery Practices", 0.25, false),
        ];
        let fillers = vec![filler("Delivery Practices", "Development work", 1.0, 2.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out, vec![]);
    }

    /// C6, `FillerService.kt:56-72`. The happy path, and the shape every other case
    /// is a deviation from.
    #[test]
    fn a_meeting_only_day_below_eight_hours_receives_a_filler_on_a_present_project() {
        let normalized = vec![agg(day(18), "Delivery Practices", 2.0, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 1.0, 1.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(
            out,
            vec![FillerEntry {
                date: day(18),
                devpro_project_name: "Delivery Practices".to_string(),
                task_title: "Development work".to_string(),
                billability: "NonBillable".to_string(),
                hours: 1.0,
            }]
        );
    }

    /// C6, `FillerService.kt:60-63`. Exactly 8h leaves `remainingHours == 0.0`, and
    /// the guard is `<= 0`, not `< 0`.
    #[test]
    fn a_day_already_at_exactly_eight_hours_receives_nothing() {
        let normalized = vec![
            agg(day(18), "Delivery Practices", 0.5, true),
            agg(day(18), "Delivery Practices", 7.5, true),
        ];
        let fillers = vec![filler("Delivery Practices", "Development work", 1.0, 1.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out, vec![]);
    }

    /// C6, `FillerService.kt:63`. A day over 8h makes `remainingHours` negative;
    /// nothing downstream would cope with that, and nothing has to.
    #[test]
    fn a_day_past_eight_hours_receives_nothing_rather_than_negative_hours() {
        let normalized = vec![agg(day(18), "Delivery Practices", 9.25, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 1.0, 1.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out, vec![]);
    }

    /// C6, `FillerService.kt:87`. 7.75h leaves exactly one increment, and the loop
    /// condition is `>=`, so the smallest fillable gap is filled.
    #[test]
    fn a_gap_of_exactly_one_quarter_is_still_filled() {
        let normalized = vec![agg(day(18), "Delivery Practices", 7.75, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 0.25, 2.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 0.25);
    }

    /// C6, `FillerService.kt:87`. A gap of 0.2h is above zero and below the
    /// increment, so the first conjunct passes and the second does not.
    #[test]
    fn a_gap_below_one_quarter_never_enters_the_loop() {
        let normalized = vec![agg(day(18), "Delivery Practices", 7.8, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 0.1, 2.0)];
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out, vec![]);
        assert_eq!(rng.offered_lens, Vec::<usize>::new());
    }

    // -------------------------------------------- present-project restriction, C6

    /// C6, `FillerService.kt:69,93`. A filler whose project did not appear in the
    /// day is not a candidate, however much room is left.
    #[test]
    fn a_filler_for_a_project_absent_from_the_day_is_never_a_candidate() {
        let normalized = vec![agg(day(18), "Delivery Practices", 2.0, true)];
        let fillers = vec![
            filler("Velocitor: NLP", "Development work", 1.0, 1.0),
            filler("Delivery Practices", "Practice work", 1.0, 1.0),
        ];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].devpro_project_name, "Delivery Practices");
        assert_eq!(out[0].task_title, "Practice work");
    }

    /// C6, `FillerService.kt:102-105`. Configured fillers that match no present
    /// project leave the candidate list empty on the first pass, and the loop
    /// breaks instead of spinning on a gap it cannot close.
    #[test]
    fn a_gap_with_no_matching_filler_breaks_out_rather_than_looping() {
        let normalized = vec![agg(day(18), "Delivery Practices", 2.0, true)];
        let fillers = vec![filler("Velocitor: NLP", "Development work", 1.0, 1.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out, vec![]);
    }

    // ------------------------------------------ one filler per project per day, C6

    /// C6, `FillerService.kt:84,94,108` — the diversity rule. Two fillers on one
    /// project are two candidates on the first pass and zero on the second.
    #[test]
    fn two_fillers_on_the_same_project_yield_only_one_entry_in_a_day() {
        let normalized = vec![agg(day(18), "Delivery Practices", 2.0, true)];
        let fillers = vec![
            filler("Delivery Practices", "Development work", 1.0, 1.0),
            filler("Delivery Practices", "Practice work", 1.0, 1.0),
        ];
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].task_title, "Development work");
        // Two candidates offered once; the second pass had none and broke.
        assert_eq!(rng.offered_lens, vec![2]);
    }

    /// C6, `FillerService.kt:92-100`. Two present projects are two independent
    /// slots, filled in the order the RNG hands them over.
    #[test]
    fn two_present_projects_each_receive_one_filler() {
        let normalized = vec![
            agg(day(18), "Delivery Practices", 1.0, true),
            agg(day(18), "Velocitor: NLP", 1.0, true),
        ];
        let fillers = vec![
            filler("Delivery Practices", "Practice work", 1.0, 1.0),
            filler("Velocitor: NLP", "Development work", 1.0, 1.0),
        ];
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].devpro_project_name, "Delivery Practices");
        assert_eq!(out[1].devpro_project_name, "Velocitor: NLP");
        assert_eq!(rng.offered_lens, vec![2, 1]);
    }

    /// C6, `FillerService.kt:107`. The RNG picks out of the *available* list, which
    /// shrinks as projects are used — so "last" means a different filler each pass.
    #[test]
    fn selection_is_made_from_the_shrinking_available_list_not_the_configured_one() {
        let normalized = vec![
            agg(day(18), "Delivery Practices", 1.0, true),
            agg(day(18), "Velocitor: NLP", 1.0, true),
        ];
        let fillers = vec![
            filler("Delivery Practices", "Practice work", 1.0, 1.0),
            filler("Velocitor: NLP", "Development work", 1.0, 1.0),
        ];
        let mut rng = ScriptedRandom::last_high();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].devpro_project_name, "Velocitor: NLP");
        assert_eq!(out[1].devpro_project_name, "Delivery Practices");
        assert_eq!(rng.offered_lens, vec![2, 1]);
    }

    // -------------------------------------------------- max_synthetic_hours cap, C6

    /// C6, `FillerService.kt:66`. The cap is the gap's ceiling: a 6h hole with a 4h
    /// cap fills 4h, not 6h.
    #[test]
    fn max_synthetic_hours_caps_the_gap_below_the_hours_actually_missing() {
        let normalized = vec![agg(day(18), "Delivery Practices", 2.0, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 0.25, 10.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_high(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 4.0);
    }

    /// C6, `FillerService.kt:66,87`. A cap of zero makes `hoursToFill` zero, and
    /// the loop never runs — the borrower gets the whole synthetic budget.
    #[test]
    fn a_max_synthetic_hours_of_zero_fills_nothing() {
        let normalized = vec![agg(day(18), "Delivery Practices", 2.0, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 0.25, 10.0)];
        let mut rng = ScriptedRandom::first_high();
        let out = generate_fillers(&normalized, &fillers, 0.0, None, &mut rng);
        assert_eq!(out, vec![]);
        assert_eq!(rng.offered_lens, Vec::<usize>::new());
    }

    /// C6, `FillerService.kt:66`. When the cap is the looser of the two, the gap
    /// itself is the limit — `min`, not "the cap wins".
    #[test]
    fn a_max_synthetic_hours_above_the_gap_leaves_the_gap_as_the_limit() {
        let normalized = vec![agg(day(18), "Delivery Practices", 6.5, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 0.25, 10.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            40.0,
            None,
            &mut ScriptedRandom::first_high(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 1.5);
    }

    // ------------------------------------------------------ quantization, C27

    /// C27, `FillerService.kt:137,165`. The filler *rounds* to the quarter. `0.375`
    /// becomes `0.5` here and `0.25` in the interactive edit path
    /// (`SettleCommand.kt:771`); a port that unified the two would return `0.25` and
    /// pass every other test in this file.
    #[test]
    fn a_degenerate_range_of_point_three_seven_five_rounds_up_to_half_an_hour() {
        let normalized = vec![agg(day(18), "Delivery Practices", 4.0, true)];
        let fillers = vec![filler(
            "Delivery Practices",
            "Development work",
            0.375,
            0.375,
        )];
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut NoDrawRandom);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 0.5);
    }

    /// C27, `FillerService.kt:165`. `0.625 / 0.25` is `2.5`, which `Math.round`
    /// sends to `3` (HALF_UP) rather than to `2` (half-to-even). Two plausible
    /// rounding rules, and they disagree here.
    #[test]
    fn a_range_of_point_six_two_five_rounds_half_up_to_three_quarters() {
        let normalized = vec![agg(day(18), "Delivery Practices", 4.0, true)];
        let fillers = vec![filler(
            "Delivery Practices",
            "Development work",
            0.625,
            0.625,
        )];
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut NoDrawRandom);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 0.75);
    }

    // -------------------------------------------- the two hour-range branches, C6

    /// C6, `FillerService.kt:130-134`. When the ceiling is not above the floor the
    /// floor is taken directly; `Random.nextDouble` would `require(until > from)`
    /// and throw on the degenerate range, so the branch is not an optimization.
    #[test]
    fn a_ceiling_equal_to_the_floor_is_taken_without_consulting_the_random_source() {
        let normalized = vec![agg(day(18), "Delivery Practices", 4.0, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 1.0, 1.0)];
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut NoDrawRandom);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 1.0);
    }

    /// C6, `FillerService.kt:122,133`. The draw's range is `[minAllowed,
    /// maxAllowed)` after both have been clamped — the ceiling by the gap, the
    /// floor by the ceiling.
    #[test]
    fn the_draw_range_is_the_clamped_floor_and_ceiling_not_the_configured_ones() {
        let normalized = vec![agg(day(18), "Delivery Practices", 6.5, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 0.5, 10.0)];
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        // Gap is 1.5h, so the ceiling is 1.5 and not the configured 10.0.
        assert_eq!(rng.draw_ranges, vec![(0.5, 1.5)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 0.5);
    }

    /// C6, `FillerService.kt:122`. A configured minimum above the maximum is pulled
    /// *down* to the maximum — `min(minHours, maxAllowed)`, which reads backwards
    /// and is what the source says.
    #[test]
    fn a_minimum_above_the_maximum_collapses_onto_the_maximum() {
        let normalized = vec![agg(day(18), "Delivery Practices", 4.0, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 5.0, 1.0)];
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut NoDrawRandom);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 1.0);
    }

    /// C6, `FillerService.kt:122,133`. A floor between zero and the increment is
    /// legal and keeps the draw range open; the result still quantizes to a
    /// quarter.
    #[test]
    fn a_floor_below_the_increment_still_produces_a_quantized_entry() {
        let normalized = vec![agg(day(18), "Delivery Practices", 7.0, true)];
        let fillers = vec![filler("Delivery Practices", "Development work", 0.1, 1.0)];
        let mut rng = ScriptedRandom::new(Pick::First, 0.5);
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(rng.draw_ranges, vec![(0.1, 1.0)]);
        assert_eq!(out.len(), 1);
        // 0.1 + 0.5 * 0.9 = 0.55 -> 2.2 quarters -> 2 -> 0.5
        assert_eq!(out[0].hours, 0.5);
    }

    // ----------------------------------- the `continue` and `break` exits, C6

    /// C6, `FillerService.kt:108,125-127`. A ceiling below the increment `continue`s
    /// — and the project was already marked used, so the next pass sees a smaller
    /// candidate set and the loop makes progress. This test is the termination
    /// proof for that branch: it returns.
    #[test]
    fn a_ceiling_below_the_increment_skips_that_filler_and_lets_the_next_one_run() {
        let normalized = vec![
            agg(day(18), "Delivery Practices", 1.0, true),
            agg(day(18), "Velocitor: NLP", 1.0, true),
        ];
        let fillers = vec![
            filler("Delivery Practices", "Practice work", 0.0, 0.1),
            filler("Velocitor: NLP", "Development work", 1.0, 1.0),
        ];
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].devpro_project_name, "Velocitor: NLP");
        // Pass 1 offered both and skipped the first; pass 2 offered only the second.
        assert_eq!(rng.offered_lens, vec![2, 1]);
    }

    /// C6, `FillerService.kt:108,125-127`. The same branch with nothing left to
    /// fall through to: the used-project marking is what stops the `continue` from
    /// looping forever on an unchanged `hoursToFill`.
    #[test]
    fn a_lone_filler_below_the_increment_terminates_instead_of_spinning() {
        let normalized = vec![agg(day(18), "Delivery Practices", 1.0, true)];
        let fillers = vec![filler("Delivery Practices", "Practice work", 0.0, 0.1)];
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out, vec![]);
        assert_eq!(rng.offered_lens, vec![1]);
    }

    /// C6, `FillerService.kt:155-158`. A draw that quantizes to zero `break`s — it
    /// does **not** `continue`. The second project is still available and still
    /// gets nothing, which is what separates this branch from the one above.
    #[test]
    fn a_draw_rounding_to_zero_ends_the_day_rather_than_trying_the_next_project() {
        let normalized = vec![
            agg(day(18), "Delivery Practices", 1.0, true),
            agg(day(18), "Velocitor: NLP", 1.0, true),
        ];
        let fillers = vec![
            filler("Delivery Practices", "Practice work", 0.0, 1.0),
            filler("Velocitor: NLP", "Development work", 1.0, 1.0),
        ];
        // Draw at the very bottom of [0.0, 1.0) -> 0.0 -> rounds to 0.0.
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out, vec![]);
        assert_eq!(rng.offered_lens, vec![2]);
    }

    /// C6, `FillerService.kt:137,157`. The zero test is on the *quantized* value,
    /// not the draw: an eighth of an hour is a positive draw that rounds to zero.
    #[test]
    fn a_positive_draw_below_an_eighth_of_an_hour_quantizes_to_zero_and_breaks() {
        let normalized = vec![agg(day(18), "Delivery Practices", 1.0, true)];
        let fillers = vec![filler("Delivery Practices", "Practice work", 0.0, 1.0)];
        // 0.0 + 0.1 * 1.0 = 0.1 -> 0.4 quarters -> Math.round(0.4) = 0.
        let mut rng = ScriptedRandom::new(Pick::First, 0.1);
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out, vec![]);
    }

    // ------------------------------------------------- filling until the gap closes

    /// C6, `FillerService.kt:87,153`. `hoursToFill` shrinks by each accepted entry,
    /// so two 1h fillers close a 2h gap and the loop stops on the condition rather
    /// than on running out of projects.
    #[test]
    fn accepted_entries_shrink_the_gap_until_the_loop_condition_fails() {
        let normalized = vec![
            agg(day(18), "A", 2.0, true),
            agg(day(18), "B", 2.0, true),
            agg(day(18), "C", 2.0, true),
        ];
        let fillers = vec![
            filler("A", "Work A", 1.0, 1.0),
            filler("B", "Work B", 1.0, 1.0),
            filler("C", "Work C", 1.0, 1.0),
        ];
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, None, &mut rng);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].task_title, "Work A");
        assert_eq!(out[1].task_title, "Work B");
        // The third project was never offered: the gap closed first.
        assert_eq!(rng.offered_lens, vec![3, 2]);
    }

    /// C6, `FillerService.kt:102-105`. When the projects run out first the gap
    /// simply stays open — the day is left short rather than over-filled.
    #[test]
    fn a_gap_wider_than_the_available_fillers_is_left_open() {
        let normalized = vec![agg(day(18), "A", 1.0, true)];
        let fillers = vec![filler("A", "Work A", 1.0, 1.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 1.0);
    }

    // ------------------------------------------------------- grouping order, C28

    /// C28, `FillerService.kt:41,43`. `groupBy` keys sit in first-encounter order,
    /// so a later date met first stays first in the output. A `BTreeMap` would sort
    /// these and a `HashMap` would shuffle them per process.
    #[test]
    fn day_groups_keep_first_encounter_order_rather_than_date_order() {
        let normalized = vec![
            agg(day(20), "A", 1.0, true),
            agg(day(18), "A", 1.0, true),
            agg(day(20), "A", 1.0, true),
        ];
        let fillers = vec![filler("A", "Work A", 1.0, 1.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].date, day(20));
        assert_eq!(out[1].date, day(18));
    }

    /// C28, `FillerService.kt:41`. Entries for one date that are not adjacent still
    /// land in one group — `groupBy` collects, it does not chunk. The 20th's two
    /// meetings sum to 2h, so its gap is 6h, not 7h.
    #[test]
    fn interleaved_dates_collect_into_one_group_each_rather_than_chunking() {
        let normalized = vec![
            agg(day(20), "A", 1.0, true),
            agg(day(18), "A", 1.0, true),
            agg(day(20), "A", 1.0, true),
        ];
        let fillers = vec![filler("A", "Work A", 0.25, 10.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            8.0,
            None,
            &mut ScriptedRandom::first_high(),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].date, day(20));
        assert_eq!(out[0].hours, 6.0);
        assert_eq!(out[1].date, day(18));
        assert_eq!(out[1].hours, 7.0);
    }

    /// C28's cheap guard. Two runs over identical input must produce identical
    /// output; a randomized map anywhere in the grouping fails this immediately.
    #[test]
    fn the_same_input_produces_the_same_output_on_a_second_run() {
        let normalized = vec![
            agg(day(20), "A", 1.0, true),
            agg(day(18), "B", 1.0, true),
            agg(day(19), "C", 1.0, true),
        ];
        let fillers = vec![
            filler("A", "Work A", 1.0, 1.0),
            filler("B", "Work B", 1.0, 1.0),
            filler("C", "Work C", 1.0, 1.0),
        ];
        let first = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        let second = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(first, second);
        assert_eq!(first.len(), 3);
    }

    // ------------------------------------------------------------- budgets, C6

    /// C6, `FillerService.kt:96`, `FillerBudgetService.kt:98-106`. A budget below
    /// the default `minRequired` of 0.25 removes the filler from the candidate set
    /// before any hours are computed.
    #[test]
    fn a_budget_below_one_quarter_removes_the_filler_from_the_candidates() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 1.0, 1.0)];
        let mut budgets = budgets_for(day(18), &[("A", "Work A", 0.2)]);
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, 4.0, Some(&mut budgets), &mut rng);
        assert_eq!(out, vec![]);
        assert_eq!(rng.offered_lens, Vec::<usize>::new());
    }

    /// C6, `FillerBudgetService.kt:106`. The comparison is `>=`, so a budget sitting
    /// exactly on the minimum is still spendable — and spends exactly one quarter.
    #[test]
    fn a_budget_of_exactly_one_quarter_still_yields_one_quarter() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 1.0, 2.0)];
        let mut budgets = budgets_for(day(18), &[("A", "Work A", 0.25)]);
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            Some(&mut budgets),
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 0.25);
    }

    /// C6, `FillerService.kt:96`. A budget of exactly zero fails `>= 0.25` and the
    /// filler never becomes a candidate.
    #[test]
    fn a_budget_of_exactly_zero_excludes_the_filler() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 1.0, 1.0)];
        let mut budgets = budgets_for(day(18), &[("A", "Work A", 0.0)]);
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            Some(&mut budgets),
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out, vec![]);
    }

    /// C6, `FillerService.kt:114-120`. The budget is a third ceiling alongside the
    /// configured maximum and the gap, and here it is the tightest of the three.
    #[test]
    fn a_budget_tighter_than_both_the_gap_and_the_maximum_becomes_the_ceiling() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 0.25, 10.0)];
        let mut budgets = budgets_for(day(18), &[("A", "Work A", 0.75)]);
        let mut rng = ScriptedRandom::first_high();
        let out = generate_fillers(&normalized, &fillers, 4.0, Some(&mut budgets), &mut rng);
        assert_eq!(rng.draw_ranges, vec![(0.25, 0.75)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 0.75);
    }

    /// C6, `FillerService.kt:141-143`, `FillerBudgetService.kt:112-130`. The spend
    /// is written back, so a second day in the same period sees a smaller budget.
    #[test]
    fn hours_spent_on_one_day_reduce_the_budget_available_to_the_next() {
        let normalized = vec![agg(day(18), "A", 7.0, true), agg(day(19), "A", 7.0, true)];
        let fillers = vec![filler("A", "Work A", 0.25, 10.0)];
        // One period covering both days.
        let mut map: Budgets = HashMap::new();
        map.insert(
            FillerKey {
                devpro_project: "A".to_string(),
                task_title: "Work A".to_string(),
            },
            1.5,
        );
        let mut budgets = period_budgets_from(
            BillingPeriod {
                start: day(16),
                end: day(30),
            },
            map,
        );

        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            Some(&mut budgets),
            &mut ScriptedRandom::first_high(),
        );
        assert_eq!(out.len(), 2);
        // Day one takes the whole 1.5h budget's worth of the 1h gap...
        assert_eq!(out[0].hours, 1.0);
        // ...leaving 0.5h for day two, which wanted 1h.
        assert_eq!(out[1].hours, 0.5);

        let left = budgets
            .for_date(day(19))
            .and_then(|b| {
                b.get(&FillerKey {
                    devpro_project: "A".to_string(),
                    task_title: "Work A".to_string(),
                })
            })
            .copied();
        assert_eq!(left, Some(0.0));
    }

    /// C6, `FillerBudgetService.kt:13,104`. The budget key is the pair, so two
    /// fillers sharing a task title on different projects have separate budgets.
    #[test]
    fn the_budget_key_is_project_and_title_together_not_the_title_alone() {
        let normalized = vec![agg(day(18), "A", 1.0, true), agg(day(18), "B", 1.0, true)];
        let fillers = vec![
            filler("A", "Development work", 1.0, 1.0),
            filler("B", "Development work", 1.0, 1.0),
        ];
        let mut budgets = budgets_for(
            day(18),
            &[
                ("A", "Development work", 0.0),
                ("B", "Development work", 5.0),
            ],
        );
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            Some(&mut budgets),
            &mut ScriptedRandom::first_low(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].devpro_project_name, "B");
    }

    /// C6, `FillerService.kt:95-97,114`. `periodBudgets == null` is the branch every
    /// real run takes today (G4), and it means unlimited — not zero.
    #[test]
    fn absent_period_budgets_mean_unlimited_rather_than_exhausted() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 0.25, 10.0)];
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            None,
            &mut ScriptedRandom::first_high(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 4.0);
    }

    /// C6, D2's third facet. A day outside every known billing period gets no map,
    /// which reads as unlimited — the same as the incumbent's `null`.
    #[test]
    fn a_day_outside_every_budget_period_is_treated_as_unlimited() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 0.25, 10.0)];
        // Budgets exist only for the 1st-15th; the 18th falls outside.
        let mut map: Budgets = HashMap::new();
        map.insert(
            FillerKey {
                devpro_project: "A".to_string(),
                task_title: "Work A".to_string(),
            },
            0.0,
        );
        let mut budgets = period_budgets_from(
            BillingPeriod {
                start: day(1),
                end: day(15),
            },
            map,
        );
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            Some(&mut budgets),
            &mut ScriptedRandom::first_high(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 4.0);
    }

    /// C6, `FillerService.kt:116-119`. A key that is not in the map reads as the
    /// unlimited sentinel, and the `< Double.MAX_VALUE` guard then skips the cap —
    /// so a filler missing from a *present* budget map behaves exactly as it does
    /// with no map at all.
    #[test]
    fn a_filler_missing_from_a_present_budget_map_is_unlimited() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 0.25, 10.0)];
        let mut budgets = budgets_for(day(18), &[("B", "Other work", 0.5)]);
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            Some(&mut budgets),
            &mut ScriptedRandom::first_high(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 4.0);
    }

    /// `FillerService.kt:145` — the `consumed >= HOUR_INCREMENT` guard is dead on
    /// every reachable path, and this test is the proof rather than a claim.
    ///
    /// The chain: `hours > 0` at `FillerService.kt:139` plus quantization at
    /// `FillerService.kt:137` makes `hours >= 0.25`; selection at `FillerService.kt:96`
    /// already required `available >= 0.25`; nothing mutates the map between the two; and
    /// `consumeBudget` returns `min(requested, available)`, so `consumed >= 0.25`. The
    /// tightest case the guard can be handed is a budget of exactly one quarter against a
    /// larger request, and it still passes.
    #[test]
    fn consumption_below_the_increment_is_unreachable_by_construction() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        // Wants 1h, may only have 0.25h.
        let fillers = vec![filler("A", "Work A", 1.0, 1.0)];
        let mut budgets = budgets_for(day(18), &[("A", "Work A", 0.25)]);
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            Some(&mut budgets),
            &mut NoDrawRandom,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 0.25);
    }

    /// C6, `FillerService.kt:137,145,151`. A period budget that is not a multiple
    /// of the increment is **overspent**, because the entry re-quantizes what the
    /// budget handed back.
    ///
    /// With 0.4h left, the ceiling is 0.4, the draw is 0.4, `roundToQuarter` lifts
    /// it to 0.5, `consumeBudget` grants only the 0.4 that exists — and then `FillerService.kt:151`
    /// rounds that 0.4 straight back up to 0.5 for the worklog. The portal gets
    /// 0.5h against a 0.4h allowance and the budget lands on zero. Incumbent
    /// behaviour, ported verbatim; only a fractional `max_hours_per_period` can
    /// reach it, and G4 says no live config sets one at all.
    #[test]
    fn a_budget_that_is_not_a_quarter_multiple_is_overspent_by_the_re_rounding() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 10.0, 10.0)];
        let mut budgets = budgets_for(day(18), &[("A", "Work A", 0.4)]);
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.0,
            Some(&mut budgets),
            &mut NoDrawRandom,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hours, 0.5);

        let left = budgets
            .for_date(day(18))
            .and_then(|b| {
                b.get(&FillerKey {
                    devpro_project: "A".to_string(),
                    task_title: "Work A".to_string(),
                })
            })
            .copied();
        assert_eq!(left, Some(0.0));
    }

    /// C6, `FillerService.kt:151`. `roundToQuarter(consumed)` is always exactly the
    /// `hours` computed at `FillerService.kt:137`, so the entry cannot disagree with what was asked
    /// for — even in the overspend case above, where `consumed` is strictly less.
    ///
    /// Why it holds: `maxAllowed <= budget` (`FillerService.kt:118`), `raw <= maxAllowed`, and
    /// `roundToQuarter` is monotone, so `hours = roundToQuarter(raw) <=
    /// roundToQuarter(budget)`; when `hours > budget` the grant is the budget and
    /// `roundToQuarter(budget)` is squeezed back onto `hours`. Recorded as an
    /// assertion rather than a comment because the two expressions look
    /// interchangeable and one of them is load-bearing for `hoursToFill`.
    #[test]
    fn the_entry_hours_always_equal_the_requested_hours_even_when_the_budget_grants_less() {
        for budget in [0.25, 0.3, 0.4, 0.49, 0.5, 0.75, 1.0, 1.1] {
            let normalized = vec![agg(day(18), "A", 2.0, true)];
            let fillers = vec![filler("A", "Work A", 10.0, 10.0)];
            let mut budgets = budgets_for(day(18), &[("A", "Work A", budget)]);
            let out = generate_fillers(
                &normalized,
                &fillers,
                4.0,
                Some(&mut budgets),
                &mut NoDrawRandom,
            );
            assert_eq!(out.len(), 1, "budget {budget}");
            // `hours` at `FillerService.kt:137` is `roundToQuarter(budget)`, since the
            // budget is the tightest of the three ceilings and the range is degenerate.
            assert_eq!(out[0].hours, round_to_quarter(budget), "budget {budget}");
        }
    }

    /// C6, `FillerService.kt:153`. The gap shrinks by the **re-rounded** grant, not
    /// by the raw one — `hoursToFill -= roundToQuarter(consumed)`, the same 0.5 the
    /// worklog carries, not the 0.4 the budget actually had.
    ///
    /// The two differ only after a fractional budget has been overspent, and only
    /// if a later filler's own ceiling then lands across a quarter boundary. Here
    /// the second filler wants everything left: 3.55h quantizes to 3.5, while the
    /// 3.65h a raw decrement would leave quantizes to 3.75. Without this case the
    /// incumbent's choice of expression is unobservable and a port may pick either.
    #[test]
    fn the_gap_shrinks_by_the_rounded_grant_not_by_the_raw_one() {
        let normalized = vec![agg(day(18), "A", 1.0, true), agg(day(18), "B", 1.0, true)];
        let fillers = vec![
            filler("A", "Work A", 10.0, 10.0),
            filler("B", "Work B", 10.0, 10.0),
        ];
        // Only A is budgeted, and at a fraction of an increment; B is unlimited.
        let mut budgets = budgets_for(day(18), &[("A", "Work A", 0.4)]);
        let out = generate_fillers(
            &normalized,
            &fillers,
            4.05,
            Some(&mut budgets),
            &mut NoDrawRandom,
        );
        assert_eq!(out.len(), 2);
        // Overspent: granted 0.4, logged 0.5.
        assert_eq!(out[0].devpro_project_name, "A");
        assert_eq!(out[0].hours, 0.5);
        // 4.05 - 0.5 = 3.55 -> 3.5. A raw `-= consumed` would leave 3.65 -> 3.75.
        assert_eq!(out[1].devpro_project_name, "B");
        assert_eq!(out[1].hours, 3.5);
    }

    // ------------------------------------------- the two injected primitives

    /// `kotlin.random.Random.nextDouble(from, until)`: the unit draw is mapped
    /// affinely onto the range, so `0.0` is the floor and the midpoint is the
    /// midpoint.
    #[test]
    fn a_unit_draw_maps_affinely_across_the_requested_range() {
        assert_eq!(kotlin_next_double_in(1.0, 3.0, 0.0), 1.0);
        assert_eq!(kotlin_next_double_in(1.0, 3.0, 0.5), 2.0);
        assert_eq!(kotlin_next_double_in(0.25, 0.75, 0.25), 0.375);
    }

    /// `Random.nextDouble`'s final `if (r >= until)` step-down. With a range whose
    /// width rounds the top draw onto the exclusive bound, the result is the ULP
    /// below it — the bound itself must never be returned.
    #[test]
    fn a_draw_never_returns_the_exclusive_upper_bound() {
        let from = 0.0;
        let until = f64::MIN_POSITIVE;
        let r = kotlin_next_double_in(from, until, NEAR_ONE);
        assert!(r < until, "draw {r} must stay below {until}");
        assert_eq!(r, f64::from_bits(until.to_bits() - 1));
    }

    /// The step-down is `nextDown`, not `to_bits() - 1`, and the two are different
    /// functions below zero. `Random.nextDouble` ends in `until.nextDown()`; the bit
    /// trick walks *toward* zero for a negative bound and, at `±0.0`, underflows the
    /// bit pattern to `u64::MAX`, i.e. NaN.
    ///
    /// **Both pairs were chosen by measurement, not by eye.** The step-down only runs
    /// when `from + unit * (until - from) >= until`, and most negative ranges do not
    /// get there: `(-1.0, -0.25, NEAR_ONE)` interpolates to `-0.2500000000000001`,
    /// already below the bound, so it returns the raw draw and proves nothing. Both
    /// pairs below were confirmed to take the branch on the JVM before being written
    /// here (`~/.cache/tt-devpro-rewrite/measurements/nextdown/Force.java`, whose
    /// re-run on 2026-09-22 shows both taking the step-down branch and the third
    /// pair not), and the expected values are `Math.nextDown`'s own output on
    /// JDK 21 rather than anything derived.
    ///
    /// The bit trick returns `-0.49999999999999994` for the first case and NaN for the
    /// second, so it fails either assertion.
    ///
    /// `fill_gap` cannot reach a non-positive `until` (`FillerService.kt:125` skips
    /// below 0.25), which is exactly why this needs its own test: nothing on the live
    /// path would ever notice.
    #[test]
    fn the_step_down_is_next_down_and_survives_a_negative_or_zero_bound() {
        // A negative range whose top draw rounds exactly onto the exclusive bound:
        // -1.0 + NEAR_ONE * 0.5 == -0.5, so `r >= until` holds and the step-down runs.
        let r = kotlin_next_double_in(-1.0, -0.5, NEAR_ONE);
        assert!(r < -0.5, "draw {r} must stay below the bound");
        assert_eq!(
            r, -0.5000000000000001,
            "Math.nextDown(-0.5) on JDK 21; the bit trick gives -0.49999999999999994"
        );

        // A bound of exactly -0.0, where the bit trick underflows to NaN. The whole
        // range is subnormal-free, so NEAR_ONE * size rounds back up to size and the
        // interpolation lands on 0.0, which is `>= -0.0`.
        let r = kotlin_next_double_in(-f64::MIN_POSITIVE, -0.0, NEAR_ONE);
        assert!(r.is_finite(), "draw must not be NaN, got {r}");
        assert!(r < 0.0, "draw {r} must stay below the bound");
        assert_eq!(r, -5e-324, "Math.nextDown(-0.0) on JDK 21");

        // The positive path, unchanged, so this test cannot pass by breaking it.
        assert_eq!(0.5f64.next_down(), 0.49999999999999994);
    }

    /// `Random.nextDouble`'s infinite-`size` branch: when `until - from` overflows
    /// to infinity while both ends are finite, each end is halved first. The naive
    /// `from + unit * (until - from)` would return NaN at `unit == 0.0`
    /// (`0.0 * ∞`).
    #[test]
    fn an_overflowing_range_is_halved_instead_of_producing_infinity() {
        let r = kotlin_next_double_in(-f64::MAX, f64::MAX, 0.0);
        assert_eq!(r, -f64::MAX);
        let mid = kotlin_next_double_in(-f64::MAX, f64::MAX, 0.5);
        assert_eq!(mid, 0.0);
    }

    /// `Random.nextDouble`'s `require(until > from)`. Unreachable from `fillGap`,
    /// which guards with `if (maxAllowed <= minAllowed)` — pinned so an
    /// implementation that silently returns `from` instead cannot creep in.
    #[test]
    #[should_panic(expected = "Random range is empty: [1.0, 1.0).")]
    fn an_empty_range_is_rejected_rather_than_collapsed_to_its_floor() {
        kotlin_next_double_in(1.0, 1.0, 0.5);
    }

    /// The message text is `RandomKt.boundsErrorMessage`'s, down to the `", "` that
    /// separates the bounds and to Java's rendering of the numbers. `1.0` is the
    /// case that distinguishes them: Rust's `{}` prints `1`. Kotlin raises an
    /// `IllegalArgumentException` where this panics, so only the text is comparable
    /// — it is pinned because a message is what a crashing run leaves behind.
    #[test]
    #[should_panic(expected = "Random range is empty: [2.0, 1.0).")]
    fn the_empty_range_message_formats_its_bounds_the_way_java_does() {
        kotlin_next_double_in(2.0, 1.0, 0.5);
    }

    /// C6, `FillerService.kt:66,111`. NaN reaching `min` is a config typo away
    /// (`max_synthetic_hours: .nan` parses), and Java's answer is NaN, which fails
    /// `hoursToFill >= HOUR_INCREMENT` and fills nothing. Rust's `f64::min` would
    /// hand back the real gap and synthesize hours instead.
    #[test]
    fn a_nan_synthetic_cap_fills_nothing_rather_than_falling_back_to_the_gap() {
        let normalized = vec![agg(day(18), "A", 2.0, true)];
        let fillers = vec![filler("A", "Work A", 1.0, 1.0)];
        let mut rng = ScriptedRandom::first_low();
        let out = generate_fillers(&normalized, &fillers, f64::NAN, None, &mut rng);
        assert_eq!(out, vec![]);
        assert_eq!(rng.offered_lens, Vec::<usize>::new());
    }
}
