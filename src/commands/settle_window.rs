//! Pure day-boundary rules for `settle`, extracted so they can be unit-tested
//! without a live CLI. Nothing here does I/O.
//!
//! Ports `commands/SettleWindow.kt`. Carries C1 (the window ends at the last
//! completed day) and C23 (the empty-state messages distinguish "settled" from
//! "nothing final yet").
//!
//! `settle` proposes hours for days that came back under 8h. Today always
//! qualifies — it is unfinished *by construction*, so it matched that predicate
//! every single morning and the filler/borrowing synthesis rounded a
//! half-finished day up to a plausible-looking 8h, sitting in the review table
//! right next to the legitimate previous day. One inattentive `[A]` published
//! that fabrication to the DevPro time report.
//!
//! Future days got in the same way: Chrono holds planned/calendar entries for
//! days that have not started, and the candidate scan had no upper bound at all.
//!
//! The rule is therefore a hard boundary, not a heuristic: the window ends at
//! the last *completed* day. Deliberately not "skip today if it looks thin" —
//! today is unfinished no matter how full it looks, and a threshold would let a
//! busy morning slip through.
//!
//! Two port notes, both about things that are easy to get wrong in Rust and
//! invisible afterwards:
//!
//! - **`today` is a parameter, never `Utc::now()` inside.** The Kotlin takes it
//!   the same way (`SettleWindow.kt:30,45`) and `SettleCommand.kt:77,248,318,335`
//!   supplies `LocalDate.now()` at the call site. Reading the clock in here would
//!   make every test below non-deterministic and the cutoff untestable.
//! - **Neither half is sorted, and neither may become sorted.** Kotlin's
//!   `partition` (`SettleWindow.kt:51`) preserves input order in both halves;
//!   C28 says ordering is an input, not a detail. The caller happens to hand in
//!   a `.distinct().sorted()` list today (`SettleCommand.kt:242-243`), which is
//!   exactly what would hide a sort introduced here.

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate, Weekday};

use crate::commands::holidays::is_us_federal_holiday;
use crate::plan::render::{ALL_CLOSED, day_label};

/// The last date `settle` may propose. Yesterday by default; `include_today`
/// moves it to today for the rare deliberate case (closing the books early
/// before time off) — never past it, since a future day has no hours at all.
///
/// Ports `SettleWindow.kt:30-31`.
pub fn last_settleable_day(today: NaiveDate, include_today: bool) -> NaiveDate {
    if include_today {
        today
    } else {
        // Kotlin's `today.minusDays(1)` throws `DateTimeException` past
        // `LocalDate.MIN`; `pred_opt` returns `None` at `NaiveDate::MIN`. Both
        // ends of the range are thousands of years outside this tool's domain.
        today
            .pred_opt()
            .expect("date underflow: today is the minimum representable date")
    }
}

/// Candidate days split by whether their hours are final.
///
/// Ports `SettleWindow.kt:34-37`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayWindow {
    pub settleable: Vec<NaiveDate>,
    pub not_final: Vec<NaiveDate>,
}

/// Splits `candidates` at the cutoff, keeping both halves: the caller says out
/// loud what it dropped rather than silently shrinking the list, because a day
/// vanishing without explanation is what sent the last investigation looking in
/// the wrong place.
///
/// Ports `SettleWindow.kt:45-53`.
pub fn split_by_finality(
    candidates: &[NaiveDate],
    today: NaiveDate,
    include_today: bool,
) -> DayWindow {
    let settle_through = last_settleable_day(today, include_today);
    let (settleable, not_final) = candidates
        .iter()
        .copied()
        .partition(|day| *day <= settle_through);
    DayWindow {
        settleable,
        not_final,
    }
}

/// The sign in front of every "not yet" note `settle` prints: a day not final, a range
/// past the cutoff, a write or a model call that is late. U+23F3 is an emoji by default,
/// so it renders as a picture with no U+FE0F after it. U+2139, the sign these notes
/// carried before, renders as a lowercase «i» in a terminal font and made «i Skipped»
/// read as a typo.
pub const NOT_YET: &str = "\u{23F3}";

/// Names the days held back, today first, suggesting `--include-today` only when today
/// is actually among them. Offering the flag for a day that no flag can unlock —
/// tomorrow, under `--include-today` — is advice that cannot be followed.
///
/// Days are named as the plan names them («Пт 9 октября»), because the note is read
/// right under the plan. Empty when nothing was held back.
pub fn describe_not_final_days(not_final: &[NaiveDate], today: NaiveDate) -> String {
    let mut sentences = Vec::new();
    if not_final.contains(&today) {
        sentences.push(format!(
            "Сегодня, {}, не планировался: часы ещё не итоговые. \
             Спланировать и его: `--include-today`.",
            day_label(today)
        ));
    }
    let future: Vec<String> = not_final
        .iter()
        .filter(|day| **day != today)
        .map(|day| day_label(*day))
        .collect();
    match future.len() {
        0 => {}
        1 => sentences.push(format!(
            "{} не планировался: день ещё не наступил.",
            future[0]
        )),
        _ => sentences.push(format!(
            "{} не планировались: дни ещё не наступили.",
            future.join(", ")
        )),
    }
    sentences.join(" ")
}

/// The empty-scan message. «Все дни закрыты» is only true when nothing was held back —
/// saying it while the note on stderr reports a held-back day gives two contradictory
/// answers to the same question on two different streams.
///
/// Ports `SettleWindow.kt:78-83`.
pub fn nothing_to_settle_message(not_final: &[NaiveDate], today: NaiveDate) -> String {
    if not_final.is_empty() {
        ALL_CLOSED.to_string()
    } else {
        format!(
            "Планировать пока нечего. {}",
            describe_not_final_days(not_final, today)
        )
    }
}

/// A day `settle` would ever plan: not a weekend and not a US federal holiday. A day
/// that is not one is never planned, final or not, so it is neither offered nor
/// reported as held back — a note about a Saturday advising `--include-today` was
/// advice the flag could not follow.
pub fn is_workday(day: NaiveDate) -> bool {
    !matches!(day.weekday(), Weekday::Sat | Weekday::Sun) && !is_us_federal_holiday(day)
}

/// `SettleCommand.kt:197` — `today.minusDays(45)`. Named here because the number
/// is the whole of the scan's lower bound and the project's own CLAUDE.md quotes
/// it ("Scans the last 45 days").
pub const SCAN_DAYS: u64 = 45;

/// A day DevPro holds at this many hours is closed.
const FULL_DAY_HOURS: f64 = 8.0;

/// `SettleCommand.kt:115`'s `ResolvedRange`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRange {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// C22. Fills in the ends the user did not type, under the rule "any date you
/// didn't type is a completed date", and returns the stderr note when the
/// resolved upper end reaches past the cutoff.
///
/// The comparison is against `cutoff`, **not** `today`: under `--include-today`
/// the cutoff *is* today, and a note saying the range runs "past the last
/// completed day" about the very day that flag just made settleable would
/// contradict itself.
///
/// The note is returned rather than printed so the two call sites
/// (`SettleCommand.kt:99`, `SettleCommand.kt:286`) can stay the only place a stream is chosen.
pub fn resolve_range(
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    today: NaiveDate,
    cutoff: NaiveDate,
) -> (ResolvedRange, Option<String>) {
    let range = ResolvedRange {
        // `LocalDate.now().withDayOfMonth(1)`. Day 1 exists in every month, so the
        // `expect` is unreachable for any date `chrono` can represent.
        from: from.unwrap_or_else(|| today.with_day(1).expect("every month has a first day")),
        to: to.unwrap_or(cutoff),
    };
    let note = if range.to > cutoff {
        Some(format!(
            "{NOT_YET} Диапазон идёт до {}, дальше последнего завершённого дня ({}): \
             часы этих дней ещё не итоговые.",
            day_label(range.to),
            day_label(cutoff)
        ))
    } else {
        None
    };
    (range, note)
}

/// C16's filter, lifted out of the fetch so it can be tested without a portal.
///
/// A day is offered when it has Chrono data, is logged under 8h in DevPro, is not
/// a weekend and is not a US federal holiday. `devpro_hours_by_day` missing a day
/// means zero hours, which is the common case — a day nobody has touched.
///
/// The comparison is `< 8.0` on the portal's own figure, not on a rounded one: a
/// day logged at 7.99h is unfilled and a day at 8.0h is not.
pub fn unfilled_days(
    settleable: &[NaiveDate],
    devpro_hours_by_day: &HashMap<NaiveDate, f64>,
) -> Vec<NaiveDate> {
    settleable
        .iter()
        .copied()
        .filter(|day| {
            let hours = devpro_hours_by_day.get(day).copied().unwrap_or(0.0);
            hours < FULL_DAY_HOURS && is_workday(*day)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(iso: &str) -> NaiveDate {
        iso.parse().expect("test date literal is a valid ISO date")
    }

    /// The fixture dates of `SettleWindowTest` (`SettleWindowTest.kt:15-17`),
    /// kept identical so a failure here points at the same Kotlin line.
    fn today() -> NaiveDate {
        d("2026-08-13")
    }
    fn yesterday() -> NaiveDate {
        d("2026-08-12")
    }
    fn tomorrow() -> NaiveDate {
        d("2026-08-14")
    }

    // -----------------------------------------------------------------------
    // The twelve cases of `SettleWindowTest`, ported one for one.
    // -----------------------------------------------------------------------

    /// C1. `SettleWindowTest.kt:20`.
    #[test]
    fn the_default_window_ends_at_yesterday() {
        assert_eq!(yesterday(), last_settleable_day(today(), false));
    }

    /// C1. `SettleWindowTest.kt:25`.
    #[test]
    fn include_today_moves_the_cutoff_to_today_and_no_further() {
        assert_eq!(today(), last_settleable_day(today(), true));
    }

    /// C1. `SettleWindowTest.kt:30`.
    #[test]
    fn today_is_dropped_by_default() {
        let window = split_by_finality(&[yesterday(), today()], today(), false);
        assert_eq!(vec![yesterday()], window.settleable);
        assert_eq!(vec![today()], window.not_final);
    }

    /// C1. `SettleWindowTest.kt:37`.
    #[test]
    fn today_is_kept_with_include_today() {
        let window = split_by_finality(&[yesterday(), today()], today(), true);
        assert_eq!(vec![yesterday(), today()], window.settleable);
        assert!(
            window.not_final.is_empty(),
            "nothing is held back once today is allowed"
        );
    }

    /// C1. `SettleWindowTest.kt:44`. Chrono holds planned entries for days that
    /// haven't started; no flag should ever make those settleable.
    #[test]
    fn a_future_day_is_dropped_even_with_include_today() {
        let window = split_by_finality(&[today(), tomorrow()], today(), true);
        assert_eq!(vec![today()], window.settleable);
        assert_eq!(vec![tomorrow()], window.not_final);
    }

    /// C1. `SettleWindowTest.kt:53`.
    #[test]
    fn past_days_are_always_kept() {
        let past = vec![d("2026-08-10"), d("2026-08-11"), d("2026-08-12")];
        let window = split_by_finality(&past, today(), false);
        assert_eq!(past, window.settleable);
        assert!(window.not_final.is_empty());
    }

    /// C1/C23. `SettleWindowTest.kt:61`. This emptiness is what decides between
    /// "all days are settled" and "nothing final yet" in the caller's
    /// empty-state message.
    #[test]
    fn not_final_is_empty_when_every_candidate_is_in_the_past() {
        let window = split_by_finality(&[yesterday()], today(), false);
        assert!(window.not_final.is_empty());
    }

    /// C1. `SettleWindowTest.kt:69`.
    #[test]
    fn everything_past_the_cutoff_is_reported_not_just_today() {
        let window = split_by_finality(&[yesterday(), today(), tomorrow()], today(), false);
        assert_eq!(vec![yesterday()], window.settleable);
        assert_eq!(vec![today(), tomorrow()], window.not_final);
    }

    /// C23. `SettleWindowTest.kt:76`.
    #[test]
    fn the_include_today_hint_appears_only_when_today_was_held_back() {
        let with_today = describe_not_final_days(&[today()], today());
        assert!(with_today.contains("Сегодня, Чт 13 августа"));
        assert!(
            with_today.contains("--include-today"),
            "the flag can actually unlock today"
        );
    }

    /// C23. `SettleWindowTest.kt:83`. Reachable under `--include-today`: today is
    /// settleable, tomorrow still isn't, and telling the user to pass the flag
    /// they already passed is advice that cannot be followed.
    #[test]
    fn a_future_only_holdback_does_not_suggest_a_flag_that_cannot_help() {
        let future_only = describe_not_final_days(&[tomorrow()], today());
        assert!(future_only.contains("Пт 14 августа"));
        assert!(
            !future_only.contains("--include-today"),
            "no flag makes a future day settleable"
        );
    }

    /// C23. `SettleWindowTest.kt:93`.
    #[test]
    fn an_empty_scan_claims_everything_is_settled_only_when_nothing_was_held_back() {
        assert_eq!(ALL_CLOSED, nothing_to_settle_message(&[], today()));
    }

    /// C23. `SettleWindowTest.kt:101`. The contradiction this guards against:
    /// stdout saying every day is closed while stderr says a day was held back.
    #[test]
    fn an_empty_scan_that_held_today_back_does_not_claim_everything_is_settled() {
        let message = nothing_to_settle_message(&[today()], today());
        assert!(
            !message.contains(ALL_CLOSED),
            "nothing was settled — today was held back"
        );
        assert!(message.contains("Чт 13 августа"));
        assert!(message.contains("--include-today"));
    }

    // -----------------------------------------------------------------------
    // The cutoff itself: the three days around it, in both modes.
    // -----------------------------------------------------------------------

    /// C1, `SettleWindow.kt:51`. The comparison is `!isAfter(cutoff)`, so the
    /// cutoff day itself is settleable. An off-by-one to `isBefore` would drop
    /// yesterday from every ordinary run and pass nine of the twelve ported
    /// cases above.
    #[test]
    fn the_cutoff_day_itself_is_settleable_by_default() {
        let cutoff = last_settleable_day(today(), false);
        let window = split_by_finality(&[cutoff], today(), false);
        assert_eq!(vec![cutoff], window.settleable);
        assert!(window.not_final.is_empty());
    }

    /// C1, `SettleWindow.kt:51`. The same boundary with the flag on: today is
    /// the cutoff and is kept.
    #[test]
    fn the_cutoff_day_itself_is_settleable_with_include_today() {
        let cutoff = last_settleable_day(today(), true);
        assert_eq!(today(), cutoff);
        let window = split_by_finality(&[cutoff], today(), true);
        assert_eq!(vec![cutoff], window.settleable);
        assert!(window.not_final.is_empty());
    }

    /// C1. One day past the cutoff is held back in either mode — by default that
    /// is today, with the flag it is tomorrow.
    #[test]
    fn the_day_after_the_cutoff_is_held_back_in_both_modes() {
        for include_today in [false, true] {
            let cutoff = last_settleable_day(today(), include_today);
            let past_cutoff = cutoff.succ_opt().unwrap();
            let window = split_by_finality(&[past_cutoff], today(), include_today);
            assert!(
                window.settleable.is_empty(),
                "cutoff+1 must never be settleable (include_today = {include_today})"
            );
            assert_eq!(vec![past_cutoff], window.not_final);
        }
    }

    /// C1. One day before the cutoff is settleable in either mode.
    #[test]
    fn the_day_before_the_cutoff_is_settleable_in_both_modes() {
        for include_today in [false, true] {
            let cutoff = last_settleable_day(today(), include_today);
            let before_cutoff = cutoff.pred_opt().unwrap();
            let window = split_by_finality(&[before_cutoff], today(), include_today);
            assert_eq!(vec![before_cutoff], window.settleable);
            assert!(window.not_final.is_empty());
        }
    }

    /// C1. All three boundary days handed in at once, default mode: the split
    /// lands exactly between cutoff and cutoff+1.
    #[test]
    fn the_three_days_around_the_default_cutoff_split_exactly_at_it() {
        let cutoff = last_settleable_day(today(), false);
        let before = cutoff.pred_opt().unwrap();
        let after = cutoff.succ_opt().unwrap();
        let window = split_by_finality(&[before, cutoff, after], today(), false);
        assert_eq!(vec![before, cutoff], window.settleable);
        assert_eq!(vec![after], window.not_final);
    }

    /// C1. Same three days with the flag on: the whole boundary moves one day
    /// later and nothing else changes.
    #[test]
    fn the_three_days_around_the_include_today_cutoff_split_exactly_at_it() {
        let cutoff = last_settleable_day(today(), true);
        let before = cutoff.pred_opt().unwrap();
        let after = cutoff.succ_opt().unwrap();
        let window = split_by_finality(&[before, cutoff, after], today(), true);
        assert_eq!(vec![before, cutoff], window.settleable);
        assert_eq!(vec![after], window.not_final);
    }

    /// C1, `SettleWindow.kt:30-31`. `--include-today` moves the cutoff to today
    /// and stops there. Stated as its own assertion because "no further" is the
    /// half of the contract that a `plusDays(1)` typo would satisfy while the
    /// ported `include-today moves the cutoff to today` case still passed on the
    /// other arm.
    #[test]
    fn no_mode_makes_tomorrow_settleable() {
        for include_today in [false, true] {
            assert!(
                last_settleable_day(today(), include_today) < tomorrow(),
                "cutoff reached tomorrow with include_today = {include_today}"
            );
            let window = split_by_finality(&[tomorrow()], today(), include_today);
            assert!(window.settleable.is_empty());
            assert_eq!(vec![tomorrow()], window.not_final);
        }
    }

    /// C1. A day from the far end of the 45-day scan window is ordinary past and
    /// is kept — the cutoff is an upper bound only, this function imposes no
    /// lower one.
    #[test]
    fn a_day_forty_five_back_is_still_settleable() {
        let far_back = d("2026-06-29");
        let window = split_by_finality(&[far_back], today(), false);
        assert_eq!(vec![far_back], window.settleable);
    }

    // -----------------------------------------------------------------------
    // Calendar arithmetic behind the default cutoff.
    // -----------------------------------------------------------------------

    /// C1. The cutoff crosses a month boundary rather than clamping inside the
    /// month.
    #[test]
    fn the_default_cutoff_crosses_into_the_previous_month() {
        assert_eq!(d("2026-08-31"), last_settleable_day(d("2026-09-01"), false));
    }

    /// C1. And a year boundary.
    #[test]
    fn the_default_cutoff_crosses_into_the_previous_year() {
        assert_eq!(d("2025-12-31"), last_settleable_day(d("2026-01-01"), false));
    }

    /// C1. March 1st of a leap year steps back onto February 29th.
    #[test]
    fn the_default_cutoff_steps_onto_the_leap_day() {
        assert_eq!(d("2028-02-29"), last_settleable_day(d("2028-03-01"), false));
    }

    /// C1. March 1st of a common year steps back onto February 28th. Paired with
    /// the case above so a hardcoded 28 or 29 fails one of them.
    #[test]
    fn the_default_cutoff_steps_onto_february_28_in_a_common_year() {
        assert_eq!(d("2026-02-28"), last_settleable_day(d("2026-03-01"), false));
    }

    /// C1. `include_today` does no arithmetic at all, on any date.
    #[test]
    fn include_today_returns_today_unchanged_on_every_boundary_date() {
        for iso in ["2026-01-01", "2026-03-01", "2028-02-29", "2026-12-31"] {
            assert_eq!(d(iso), last_settleable_day(d(iso), true));
        }
    }

    // -----------------------------------------------------------------------
    // Ordering and shape — C28.
    // -----------------------------------------------------------------------

    /// C28. `partition` preserves input order; nothing here sorts. The caller
    /// hands in a `.distinct().sorted()` list (`SettleCommand.kt:242-243`),
    /// which is exactly what would hide a sort introduced in this function, so
    /// the test feeds an unsorted one.
    #[test]
    fn the_settleable_half_keeps_input_order_rather_than_sorting() {
        let scrambled = vec![d("2026-08-12"), d("2026-08-10"), d("2026-08-11")];
        let window = split_by_finality(&scrambled, today(), false);
        assert_eq!(scrambled, window.settleable);
    }

    /// C28. The held-back half keeps input order too — it is rendered verbatim
    /// into the skip notice at `SettleCommand.kt:250`.
    #[test]
    fn the_not_final_half_keeps_input_order_rather_than_sorting() {
        let window = split_by_finality(&[tomorrow(), today()], today(), false);
        assert!(window.settleable.is_empty());
        assert_eq!(vec![tomorrow(), today()], window.not_final);
    }

    /// C28. Interleaved candidates keep their relative order within each half,
    /// which a partition built by two filtered passes would also do and a
    /// partition built via a `HashSet` would not.
    #[test]
    fn interleaved_candidates_keep_their_relative_order_within_each_half() {
        let candidates = vec![
            tomorrow(),
            d("2026-08-11"),
            today(),
            d("2026-08-09"),
            d("2026-08-15"),
            d("2026-08-10"),
        ];
        let window = split_by_finality(&candidates, today(), false);
        assert_eq!(
            vec![d("2026-08-11"), d("2026-08-09"), d("2026-08-10")],
            window.settleable
        );
        assert_eq!(vec![tomorrow(), today(), d("2026-08-15")], window.not_final);
    }

    /// C28. Duplicates survive in both halves. Kotlin's `partition` does not
    /// deduplicate, and a port routing through a set would silently collapse
    /// them — invisible today only because the caller deduplicates first.
    #[test]
    fn duplicate_candidates_are_not_collapsed() {
        let window = split_by_finality(
            &[yesterday(), yesterday(), today(), today()],
            today(),
            false,
        );
        assert_eq!(vec![yesterday(), yesterday()], window.settleable);
        assert_eq!(vec![today(), today()], window.not_final);
    }

    /// C28, the cheap guard the plan asks for: the same input run twice produces
    /// byte-identical results. Fails instantly if a randomized `HashMap`/`HashSet`
    /// ever reaches this path.
    #[test]
    fn the_same_input_twice_produces_identical_output() {
        let candidates = vec![
            d("2026-08-15"),
            d("2026-08-09"),
            today(),
            d("2026-08-11"),
            tomorrow(),
            d("2026-08-10"),
        ];
        let first = split_by_finality(&candidates, today(), false);
        let second = split_by_finality(&candidates, today(), false);
        assert_eq!(first, second);
        assert_eq!(
            describe_not_final_days(&first.not_final, today()),
            describe_not_final_days(&second.not_final, today())
        );
        assert_eq!(
            nothing_to_settle_message(&first.not_final, today()),
            nothing_to_settle_message(&second.not_final, today())
        );
    }

    // -----------------------------------------------------------------------
    // Degenerate inputs.
    // -----------------------------------------------------------------------

    /// C1. An empty scan yields two empty halves in both modes — and, through
    /// C23, the "all settled" message rather than a holdback notice.
    #[test]
    fn an_empty_candidate_list_yields_two_empty_halves() {
        for include_today in [false, true] {
            let window = split_by_finality(&[], today(), include_today);
            assert!(window.settleable.is_empty());
            assert!(window.not_final.is_empty());
        }
    }

    /// C1. Every candidate in the future: nothing is settleable and every day is
    /// reported, in order. This is the shape that produced the original
    /// fabricated day, so it gets its own case rather than riding on the mixed
    /// one.
    #[test]
    fn an_all_future_candidate_list_settles_nothing_and_reports_everything() {
        let future = vec![tomorrow(), d("2026-08-15"), d("2026-09-01")];
        let window = split_by_finality(&future, today(), true);
        assert!(window.settleable.is_empty());
        assert_eq!(future, window.not_final);
    }

    // -----------------------------------------------------------------------
    // C23 — the exact strings, not just the substrings above.
    // -----------------------------------------------------------------------

    /// The whole sentence for today: the day named as the plan names it, the reason,
    /// and the flag in backticks, since the note is pasted into a markdown chat.
    #[test]
    fn the_holdback_sentence_with_today_reads_in_full() {
        assert_eq!(
            "Сегодня, Чт 13 августа, не планировался: часы ещё не итоговые. \
             Спланировать и его: `--include-today`.",
            describe_not_final_days(&[today()], today())
        );
    }

    /// The future-only arm, one day, in full.
    #[test]
    fn the_holdback_sentence_for_one_future_day_reads_in_full() {
        assert_eq!(
            "Пт 14 августа не планировался: день ещё не наступил.",
            describe_not_final_days(&[tomorrow()], today())
        );
    }

    /// Several future days take the plural and are joined in input order.
    #[test]
    fn several_future_days_take_the_plural() {
        assert_eq!(
            "Пт 14 августа, Сб 15 августа не планировались: дни ещё не наступили.",
            describe_not_final_days(&[tomorrow(), d("2026-08-15")], today())
        );
    }

    /// Today and a future day: today's sentence comes first, whatever the input order,
    /// and the hint is decided by today being in the list, not by it being first.
    #[test]
    fn today_is_named_first_even_when_it_is_not_first_in_the_list() {
        let expected = "Сегодня, Чт 13 августа, не планировался: часы ещё не итоговые. \
                        Спланировать и его: `--include-today`. \
                        Пт 14 августа не планировался: день ещё не наступил.";
        assert_eq!(
            expected,
            describe_not_final_days(&[today(), tomorrow()], today())
        );
        assert_eq!(
            expected,
            describe_not_final_days(&[tomorrow(), today()], today())
        );
    }

    /// Nothing held back says nothing. The incumbent rendered a sentence with no
    /// subject here; no caller reaches it, and an empty string is what it means.
    #[test]
    fn an_empty_holdback_list_says_nothing() {
        assert_eq!("", describe_not_final_days(&[], today()));
    }

    /// C23, `SettleWindow.kt:82`. The holdback message in full: the prefix, then the
    /// sentence `describe_not_final_days` built.
    #[test]
    fn the_holdback_message_prefixes_the_sentence_with_nothing_to_plan_yet() {
        assert_eq!(
            "Планировать пока нечего. Сегодня, Чт 13 августа, не планировался: часы ещё не \
             итоговые. Спланировать и его: `--include-today`.",
            nothing_to_settle_message(&[today()], today())
        );
    }

    /// C23. "Every day is closed" is keyed on the holdback list being empty, not on
    /// today being absent from it: a future-only holdback must not claim it either.
    #[test]
    fn a_future_only_holdback_still_refuses_to_claim_everything_is_settled() {
        let message = nothing_to_settle_message(&[tomorrow()], today());
        assert_eq!(
            "Планировать пока нечего. Пт 14 августа не планировался: день ещё не наступил.",
            message
        );
        assert!(!message.contains("--include-today"));
    }

    /// U+23F3 is an emoji by default and needs no U+FE0F; U+2139, the sign before
    /// it, rendered as a lowercase «i» in the terminal font.
    #[test]
    fn the_not_yet_sign_is_the_hourglass() {
        assert_eq!(NOT_YET, "\u{23F3}");
    }

    /// A weekend or a holiday is never planned, so it is never reported as held back.
    #[test]
    fn a_workday_is_a_weekday_that_is_not_a_holiday() {
        assert!(is_workday(today()), "Thursday");
        assert!(!is_workday(d("2026-08-15")), "Saturday");
        assert!(!is_workday(d("2026-08-16")), "Sunday");
        assert!(!is_workday(d("2026-09-07")), "Labor Day");
    }

    // -----------------------------------------------------------------------
    // The two halves wired together, as `SettleCommand` uses them.
    // -----------------------------------------------------------------------

    /// C1 + C23, `SettleCommand.kt:248-250` and `SettleCommand.kt:335`. The end-to-end shape of
    /// an ordinary morning: yesterday and today scanned, today held back, and the
    /// two messages that come out of it agreeing with each other.
    #[test]
    fn an_ordinary_morning_settles_yesterday_and_explains_today() {
        let window = split_by_finality(&[yesterday(), today()], today(), false);
        assert_eq!(vec![yesterday()], window.settleable);
        assert_eq!(
            format!(
                "Планировать пока нечего. {}",
                describe_not_final_days(&window.not_final, today())
            ),
            nothing_to_settle_message(&window.not_final, today())
        );
        assert!(describe_not_final_days(&window.not_final, today()).starts_with("Сегодня, Чт 13"));
    }

    /// C1 + C23. The same morning with `--include-today`: today becomes
    /// settleable, nothing is held back, and the empty-scan message flips to the
    /// settled one. This is the pair that makes the two messages a contract
    /// rather than two independent strings.
    #[test]
    fn the_same_morning_with_include_today_holds_nothing_back_and_reports_settled() {
        let window = split_by_finality(&[yesterday(), today()], today(), true);
        assert_eq!(vec![yesterday(), today()], window.settleable);
        assert!(window.not_final.is_empty());
        assert_eq!(
            ALL_CLOSED,
            nothing_to_settle_message(&window.not_final, today())
        );
    }

    /// C1 + C23. A run under `--include-today` that still has a planned Chrono
    /// day in it: today settles, tomorrow is held back, and the notice
    /// deliberately does not suggest the flag that is already on.
    #[test]
    fn include_today_with_a_planned_future_day_reports_it_without_suggesting_the_flag() {
        let window = split_by_finality(&[today(), tomorrow()], today(), true);
        assert_eq!(vec![today()], window.settleable);
        let notice = describe_not_final_days(&window.not_final, today());
        assert_eq!(
            "Пт 14 августа не планировался: день ещё не наступил.",
            notice
        );
    }

    // -----------------------------------------------------------------------
    // C22 — the explicit range
    // -----------------------------------------------------------------------

    /// `LocalDate.now().withDayOfMonth(1)`, and not "45 days back" — the two
    /// coincide for no month.
    #[test]
    fn an_unspecified_from_is_the_first_of_the_month_today_falls_in() {
        let (range, note) = resolve_range(
            None,
            Some(d("2026-09-20")),
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert_eq!(range.from, d("2026-09-01"));
        assert_eq!(note, None);
    }

    /// The upper default is the cutoff, which under `--include-today` is today and
    /// otherwise yesterday. A port defaulting to `today` settles an unfinished day.
    #[test]
    fn an_unspecified_to_is_the_cutoff_and_not_today() {
        let (range, _) = resolve_range(
            Some(d("2026-09-01")),
            None,
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert_eq!(range.to, d("2026-09-21"));

        let (included, _) = resolve_range(
            Some(d("2026-09-01")),
            None,
            d("2026-09-22"),
            d("2026-09-22"),
        );
        assert_eq!(included.to, d("2026-09-22"));
    }

    /// C22's rule: an explicit range is honoured verbatim and the note is
    /// non-blocking. A port that clamped `to` to the cutoff would pass every other
    /// test here.
    #[test]
    fn a_to_past_the_cutoff_is_honoured_and_carries_the_stderr_note() {
        let (range, note) = resolve_range(
            Some(d("2026-09-01")),
            Some(d("2026-09-30")),
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert_eq!(range.to, d("2026-09-30"));
        assert_eq!(
            note.as_deref(),
            Some(
                "\u{23F3} Диапазон идёт до Ср 30 сентября, дальше последнего завершённого дня \
                 (Пн 21 сентября): часы этих дней ещё не итоговые."
            )
        );
    }

    /// The boundary. `>` and not `>=`, so the cutoff itself is silent.
    #[test]
    fn a_to_exactly_on_the_cutoff_produces_no_note() {
        let (_, note) = resolve_range(
            None,
            Some(d("2026-09-21")),
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert_eq!(note, None);
    }

    /// The comparison is against the cutoff, not today. Under `--include-today` the
    /// two are the same date, and a port comparing with `today` would announce that
    /// the range runs past the last completed day about the very day the flag just
    /// made settleable.
    #[test]
    fn under_include_today_a_to_of_today_produces_no_note() {
        let (range, note) = resolve_range(
            None,
            Some(d("2026-09-22")),
            d("2026-09-22"),
            d("2026-09-22"),
        );
        assert_eq!(range.to, d("2026-09-22"));
        assert_eq!(note, None);

        let (_, without_the_flag) = resolve_range(
            None,
            Some(d("2026-09-22")),
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert!(
            without_the_flag.is_some(),
            "the premise: without the flag the same date is noted"
        );
    }

    // -----------------------------------------------------------------------
    // C16 — which days are offered
    // -----------------------------------------------------------------------

    /// The comparison is `< 8.0` on the portal's own figure. A day at exactly 8h is
    /// settled; a hundredth under is not.
    #[test]
    fn a_day_at_exactly_eight_hours_is_settled_and_a_hundredth_under_is_not() {
        let days = vec![d("2026-09-15"), d("2026-09-16")];
        let mut logged = HashMap::new();
        logged.insert(d("2026-09-15"), 8.0);
        logged.insert(d("2026-09-16"), 7.99);
        assert_eq!(unfilled_days(&days, &logged), vec![d("2026-09-16")]);
    }

    /// A day nobody has touched is missing from the portal's map, which is zero
    /// hours and therefore unfilled — the common case, and the one an
    /// `unwrap_or(8.0)` style default would silently drop.
    #[test]
    fn a_day_the_portal_never_mentioned_counts_as_zero_hours_and_is_offered() {
        let days = vec![d("2026-09-15")];
        assert_eq!(unfilled_days(&days, &HashMap::new()), vec![d("2026-09-15")]);
    }

    /// 2026-09-19 is a Saturday and 2026-09-20 a Sunday. Neither is ever offered,
    /// whatever the portal says about them.
    #[test]
    fn weekends_are_never_offered_even_at_zero_hours() {
        let days = vec![
            d("2026-09-18"),
            d("2026-09-19"),
            d("2026-09-20"),
            d("2026-09-21"),
        ];
        assert_eq!(
            unfilled_days(&days, &HashMap::new()),
            vec![d("2026-09-18"), d("2026-09-21")]
        );
    }

    /// 2026-09-07 is Labor Day. The holiday predicate is consulted for the same
    /// reason the weekend test is: neither day is expected to hold eight hours.
    #[test]
    fn a_us_federal_holiday_is_never_offered() {
        let days = vec![d("2026-09-04"), d("2026-09-07"), d("2026-09-08")];
        assert_eq!(
            unfilled_days(&days, &HashMap::new()),
            vec![d("2026-09-04"), d("2026-09-08")]
        );
    }

    /// The input order is kept — the day-by-day listing prints these in the order
    /// this returns them.
    #[test]
    fn the_offered_days_keep_the_order_they_arrived_in() {
        let days = vec![d("2026-09-18"), d("2026-09-15"), d("2026-09-16")];
        assert_eq!(unfilled_days(&days, &HashMap::new()), days);
    }
}
