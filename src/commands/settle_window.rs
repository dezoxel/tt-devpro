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

use chrono::NaiveDate;

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

/// Renders a not-final day, marking today so the reason is obvious.
///
/// Ports `SettleWindow.kt:56-57`. The bare date comes from `LocalDate.toString()`,
/// which is ISO-8601 with zero-padded month and day — `NaiveDate`'s `Display` is
/// the same.
pub fn describe_not_final(day: NaiveDate, today: NaiveDate) -> String {
    if day == today {
        format!("{day} (today)")
    } else {
        format!("{day}")
    }
}

/// Names the days held back, suggesting `--include-today` only when today is
/// actually among them. Offering the flag for a day that no flag can unlock —
/// tomorrow, under `--include-today` — is advice that cannot be followed.
///
/// Ports `SettleWindow.kt:64-71`. The dash in the second arm is U+2014 and the
/// apostrophe is ASCII; both are copied out of the Kotlin source by bytes.
pub fn describe_not_final_days(not_final: &[NaiveDate], today: NaiveDate) -> String {
    let listed = not_final
        .iter()
        .map(|day| describe_not_final(*day, today))
        .collect::<Vec<_>>()
        .join(", ");
    if not_final.contains(&today) {
        format!("{listed}. Use --include-today to settle today anyway.")
    } else {
        format!("{listed} — those days haven't happened yet.")
    }
}

/// The empty-scan message. "All days are settled" is only true when nothing was
/// held back — saying it while the skip notice reports a dropped day gives two
/// contradictory answers to the same question on two different streams.
///
/// Ports `SettleWindow.kt:78-83`. The `≥` is U+2265 with no space before the `8`.
pub fn nothing_to_settle_message(not_final: &[NaiveDate], today: NaiveDate) -> String {
    if not_final.is_empty() {
        "All days are settled (≥8h logged).".to_string()
    } else {
        format!(
            "Nothing to settle yet. Held back: {}",
            describe_not_final_days(not_final, today)
        )
    }
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
        assert!(with_today.contains("2026-08-13 (today)"));
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
        assert!(future_only.contains("2026-08-14"));
        assert!(
            !future_only.contains("--include-today"),
            "no flag makes a future day settleable"
        );
    }

    /// C23. `SettleWindowTest.kt:93`.
    #[test]
    fn an_empty_scan_claims_everything_is_settled_only_when_nothing_was_held_back() {
        assert_eq!(
            "All days are settled (≥8h logged).",
            nothing_to_settle_message(&[], today())
        );
    }

    /// C23. `SettleWindowTest.kt:101`. The contradiction this guards against:
    /// stdout saying "all settled" while stderr says a day was skipped.
    #[test]
    fn an_empty_scan_that_held_today_back_does_not_claim_everything_is_settled() {
        let message = nothing_to_settle_message(&[today()], today());
        assert!(
            !message.contains("All days are settled"),
            "nothing was settled — today was held back"
        );
        assert!(message.contains("2026-08-13 (today)"));
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
    // C23 — the exact strings, not just the substrings the Kotlin asserted.
    // -----------------------------------------------------------------------

    /// C23, `SettleWindow.kt:56-57`. Today carries the ` (today)` marker.
    #[test]
    fn describe_not_final_marks_today() {
        assert_eq!("2026-08-13 (today)", describe_not_final(today(), today()));
    }

    /// C23, `SettleWindow.kt:57`. Any other day renders bare, with no marker —
    /// including a past one, which the caller never produces but the function
    /// accepts.
    #[test]
    fn describe_not_final_renders_any_other_day_bare() {
        assert_eq!("2026-08-14", describe_not_final(tomorrow(), today()));
        assert_eq!("2026-08-12", describe_not_final(yesterday(), today()));
    }

    /// C23. The date is ISO-8601 with zero-padded month and day, because that is
    /// what `LocalDate.toString()` emits and the skip notice is compared against
    /// the incumbent's stderr byte for byte.
    #[test]
    fn describe_not_final_pads_single_digit_months_and_days() {
        assert_eq!("2026-01-05", describe_not_final(d("2026-01-05"), today()));
    }

    /// C23, `SettleWindow.kt:67`. The whole sentence, not a substring: a port
    /// that dropped the trailing period or reworded the hint would pass the
    /// ported `contains` case above.
    #[test]
    fn the_holdback_sentence_with_today_reads_exactly_as_the_incumbent_prints_it() {
        assert_eq!(
            "2026-08-13 (today). Use --include-today to settle today anyway.",
            describe_not_final_days(&[today()], today())
        );
    }

    /// C23, `SettleWindow.kt:69`. The future-only arm, in full. The dash is
    /// U+2014 and the apostrophe is ASCII; both are copied out of the Kotlin
    /// source by bytes.
    #[test]
    fn the_holdback_sentence_without_today_reads_exactly_as_the_incumbent_prints_it() {
        assert_eq!(
            "2026-08-14 — those days haven't happened yet.",
            describe_not_final_days(&[tomorrow()], today())
        );
    }

    /// C23, `SettleWindow.kt:69`. The em dash is U+2014, not a hyphen and not an
    /// en dash, and the apostrophe is U+0027, not U+2019. Both are invisible in
    /// an editor and both would break a byte diff against the incumbent's
    /// stderr.
    #[test]
    fn the_future_only_arm_uses_an_em_dash_and_an_ascii_apostrophe() {
        let sentence = describe_not_final_days(&[tomorrow()], today());
        assert!(sentence.contains('\u{2014}'), "em dash U+2014 is missing");
        assert!(!sentence.contains('\u{2013}'), "en dash U+2013 crept in");
        assert!(sentence.contains("haven't"), "ASCII apostrophe is missing");
        assert!(!sentence.contains('\u{2019}'), "curly apostrophe crept in");
    }

    /// C23, `SettleWindow.kt:65`. Several held-back days are joined with `", "`
    /// in input order, and the today marker applies per day.
    #[test]
    fn several_held_back_days_are_joined_with_a_comma_and_a_space() {
        assert_eq!(
            "2026-08-13 (today), 2026-08-14. Use --include-today to settle today anyway.",
            describe_not_final_days(&[today(), tomorrow()], today())
        );
    }

    /// C23, `SettleWindow.kt:66`. The hint is decided by `notFinal.contains(today)`,
    /// not by today being first — an implementation keying off the head of the
    /// list would drop the hint here.
    #[test]
    fn the_hint_fires_when_today_is_not_the_first_held_back_day() {
        assert_eq!(
            "2026-08-14, 2026-08-13 (today). Use --include-today to settle today anyway.",
            describe_not_final_days(&[tomorrow(), today()], today())
        );
    }

    /// C23. A holdback list of nothing but future days, plural, takes the second
    /// arm and names them all.
    #[test]
    fn several_future_days_take_the_no_flag_can_help_arm() {
        assert_eq!(
            "2026-08-14, 2026-08-15 — those days haven't happened yet.",
            describe_not_final_days(&[tomorrow(), d("2026-08-15")], today())
        );
    }

    /// C23, `SettleWindow.kt:64-71`. **A finding, pinned rather than fixed.** An
    /// empty `notFinal` takes the `else` arm and renders a sentence with an empty
    /// subject and a leading space. Unreachable from either live call site —
    /// `SettleCommand.kt:249` guards on `isNotEmpty()` and
    /// `nothingToSettleMessage` checks `isEmpty()` first — but it is the
    /// incumbent's behaviour for this input and parity is the bar.
    #[test]
    fn an_empty_holdback_list_renders_the_incumbents_subjectless_sentence() {
        assert_eq!(
            " — those days haven't happened yet.",
            describe_not_final_days(&[], today())
        );
    }

    /// C23, `SettleWindow.kt:80`. The settled message in full, including the
    /// closing period.
    #[test]
    fn the_settled_message_reads_exactly_as_the_incumbent_prints_it() {
        assert_eq!(
            "All days are settled (≥8h logged).",
            nothing_to_settle_message(&[], today())
        );
    }

    /// C23, `SettleWindow.kt:80`. The glyph is U+2265, not the ASCII `>=` a
    /// retyped port would produce, and there is no space between it and the `8`.
    #[test]
    fn the_settled_message_uses_the_greater_or_equal_glyph_with_no_space_after_it() {
        let message = nothing_to_settle_message(&[], today());
        assert!(message.contains('\u{2265}'), "U+2265 is missing");
        assert!(!message.contains(">="), "ASCII >= crept in");
        assert!(
            message.contains("\u{2265}8h"),
            "a space crept in after U+2265"
        );
    }

    /// C23, `SettleWindow.kt:82`. The holdback message in full: the prefix, then
    /// the sentence `describeNotFinalDays` built.
    #[test]
    fn the_holdback_message_prefixes_the_sentence_with_nothing_to_settle_yet() {
        assert_eq!(
            "Nothing to settle yet. Held back: 2026-08-13 (today). Use --include-today to settle today anyway.",
            nothing_to_settle_message(&[today()], today())
        );
    }

    /// C23. The same, for the arm no flag can unlock.
    #[test]
    fn the_holdback_message_carries_the_future_only_arm_unchanged() {
        assert_eq!(
            "Nothing to settle yet. Held back: 2026-08-14 — those days haven't happened yet.",
            nothing_to_settle_message(&[tomorrow()], today())
        );
    }

    /// C23. "All days are settled" is keyed on the holdback list being empty, not
    /// on today being absent from it: a future-only holdback must not claim
    /// everything is settled either.
    #[test]
    fn a_future_only_holdback_still_refuses_to_claim_everything_is_settled() {
        let message = nothing_to_settle_message(&[tomorrow()], today());
        assert!(!message.contains("All days are settled"));
        assert!(!message.contains("--include-today"));
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
            "2026-08-13 (today). Use --include-today to settle today anyway.",
            describe_not_final_days(&window.not_final, today())
        );
        assert_eq!(
            "Nothing to settle yet. Held back: 2026-08-13 (today). Use --include-today to settle today anyway.",
            nothing_to_settle_message(&window.not_final, today())
        );
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
            "All days are settled (≥8h logged).",
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
        assert_eq!("2026-08-14 — those days haven't happened yet.", notice);
    }
}
