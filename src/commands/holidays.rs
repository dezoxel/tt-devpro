//! US federal holiday calendar.
//!
//! Ports `commands/Holidays.kt` — the pure predicate behind `settle`'s
//! "unfilled workday" scan (C4). Nothing here does I/O.
//!
//! Fixed-date holidays are *observed* on an adjacent weekday when they fall on a
//! weekend: a Saturday holiday is observed the preceding Friday, a Sunday holiday
//! the following Monday. The scan must treat the observed day as off too,
//! otherwise it pads a real day-off with fabricated hours — Jul 4 2026 is a
//! Saturday, so the day off is Friday Jul 3.
//!
//! Floating holidays (MLK, Memorial, Labor, Thanksgiving) are defined as an
//! "nth weekday of the month", so they can never land on a weekend and need no
//! shifting.
//!
//! **The list is eight holidays; the real US federal calendar has eleven.**
//! Presidents' Day (3rd Mon Feb), Columbus Day (2nd Mon Oct) and Veterans Day
//! (Nov 11, fixed-date and therefore needing the observance shift) are absent
//! from `Holidays.kt` and stay absent here — parity is the bar, not the
//! correctness of the calendar. Completing the set is the most plausible
//! unrequested improvement anyone would make to this file, and it would silently
//! stop the scan offering three real workdays a year, so
//! `presidents_day_columbus_day_and_veterans_day_are_deliberately_absent` exists
//! to make that edit fail loudly rather than pass quietly.
//!
//! `java.time.TemporalAdjusters` has no `chrono` counterpart, so
//! `dayOfWeekInMonth(n, dow)` and `lastInMonth(dow)` are hand-written below.
//! Both are tested against values measured on a JVM, not derived.

use chrono::{Datelike, Days, NaiveDate, Weekday};

/// Weekend-observance shift for a fixed-date holiday. Ports `Holidays.kt:23-27`.
///
/// `java.time`'s calendar is effectively unbounded, `chrono`'s ends at ±262 143.
/// At those two dates Kotlin would throw and this returns the input unshifted.
/// Unreachable from any Chrono or portal date; the alternative is a panic inside
/// a pure predicate.
fn observed(actual: NaiveDate) -> NaiveDate {
    match actual.weekday() {
        Weekday::Sat => actual.pred_opt().unwrap_or(actual),
        Weekday::Sun => actual.succ_opt().unwrap_or(actual),
        _ => actual,
    }
}

/// `java.time.temporal.TemporalAdjusters.dayOfWeekInMonth(n, dow)` applied to the
/// 1st of `month`, for `n >= 1`.
///
/// Java's rule is plain arithmetic — first matching weekday on or after the 1st,
/// plus `n - 1` weeks — with **no clamping to the month**: measured on JDK,
/// `dayOfWeekInMonth(5, MONDAY)` on February 2026 returns `2026-03-02`. The four
/// call sites here use `n` of 1, 3 and 4 and never leave their month, but the
/// helper reproduces the roll-over rather than inventing a clamp.
///
/// `None` only where `chrono`'s calendar ends.
fn nth_weekday_in_month(year: i32, month: u32, weekday: Weekday, n: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let to_first_match =
        (7 + weekday.num_days_from_monday() - first.weekday().num_days_from_monday()) % 7;
    first.checked_add_days(Days::new(u64::from(to_first_match + (n - 1) * 7)))
}

/// `java.time.temporal.TemporalAdjusters.lastInMonth(dow)` applied to `month`.
///
/// `None` only where `chrono`'s calendar ends.
fn last_weekday_in_month(year: i32, month: u32, weekday: Weekday) -> Option<NaiveDate> {
    let first_of_next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    let last_of_month = first_of_next.pred_opt()?;
    let back =
        (7 + last_of_month.weekday().num_days_from_monday() - weekday.num_days_from_monday()) % 7;
    last_of_month.checked_sub_days(Days::new(u64::from(back)))
}

/// C4. Ports `Holidays.kt:29-58`.
pub fn is_us_federal_holiday(date: NaiveDate) -> bool {
    let year = date.year();

    // Fixed-date holidays. Next year's New Year is included because a Jan 1 that
    // falls on a Saturday is observed on Dec 31 of the *current* year — the only
    // fixed holiday adjacent to a year boundary. A day matches if it is either
    // the literal date or its observed shift.
    let fixed = [
        NaiveDate::from_ymd_opt(year, 1, 1),
        NaiveDate::from_ymd_opt(year + 1, 1, 1),
        NaiveDate::from_ymd_opt(year, 6, 19),  // Juneteenth
        NaiveDate::from_ymd_opt(year, 7, 4),   // Independence Day
        NaiveDate::from_ymd_opt(year, 12, 25), // Christmas
    ];
    if fixed
        .into_iter()
        .flatten()
        .any(|f| date == f || date == observed(f))
    {
        return true;
    }

    // Floating holidays — never a weekend, so no observed shift.
    let floating = [
        nth_weekday_in_month(year, 1, Weekday::Mon, 3), // MLK Jr. Day
        last_weekday_in_month(year, 5, Weekday::Mon),   // Memorial Day
        nth_weekday_in_month(year, 9, Weekday::Mon, 1), // Labor Day
        nth_weekday_in_month(year, 11, Weekday::Thu, 4), // Thanksgiving
    ];
    floating.into_iter().flatten().any(|f| date == f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(iso: &str) -> NaiveDate {
        NaiveDate::parse_from_str(iso, "%Y-%m-%d").expect("test date")
    }

    /// Mirrors `HolidayTest.holiday(date)` (`HolidayTest.kt:11`).
    fn h(iso: &str) -> bool {
        is_us_federal_holiday(d(iso))
    }

    fn assert_holidays(days: &[&str]) {
        for day in days {
            assert!(h(day), "{day} should be a holiday");
        }
    }

    fn assert_workdays(days: &[&str]) {
        for day in days {
            assert!(!h(day), "{day} should NOT be a holiday");
        }
    }

    // ---------------------------------------------------------------------
    // The seven cases of `HolidayTest.kt`, ported verbatim.
    // ---------------------------------------------------------------------

    /// C4. `HolidayTest.kt:14-19`.
    #[test]
    fn saturday_holiday_is_observed_the_preceding_friday() {
        // Jul 4 2026 is a Saturday → observed Friday Jul 3.
        assert!(h("2026-07-03"), "Jul 3 2026 (observed Independence Day)");
        assert!(h("2026-07-04"), "Jul 4 2026 (literal Independence Day)");
    }

    /// C4. `HolidayTest.kt:21-26`.
    #[test]
    fn weekday_holiday_does_not_shift() {
        // Jul 4 2025 is a Friday — the day itself, nothing adjacent.
        assert!(h("2025-07-04"), "Jul 4 2025 (Fri)");
        assert!(!h("2025-07-03"), "Jul 3 2025 (Thu) is a normal workday");
    }

    /// C4. `HolidayTest.kt:28-33`.
    #[test]
    fn sunday_holiday_is_observed_the_following_monday() {
        // Dec 25 2022 is a Sunday → observed Monday Dec 26.
        assert!(h("2022-12-25"), "Dec 25 2022 (literal Christmas)");
        assert!(h("2022-12-26"), "Dec 26 2022 (observed Christmas)");
    }

    /// C4. `HolidayTest.kt:35-40`.
    #[test]
    fn new_year_on_saturday_is_observed_on_dec_31_of_the_prior_year() {
        // Jan 1 2022 is a Saturday → observed Friday Dec 31 2021 (cross-year).
        assert!(h("2022-01-01"), "Jan 1 2022 (literal New Year)");
        assert!(h("2021-12-31"), "Dec 31 2021 (observed New Year)");
    }

    /// C4. `HolidayTest.kt:42-48`.
    #[test]
    fn juneteenth_is_a_fixed_holiday_and_shifts_on_weekends() {
        assert!(h("2026-06-19"), "Jun 19 2026 (Fri) literal");
        // Jun 19 2021 is a Saturday → observed Friday Jun 18.
        assert!(h("2021-06-19"), "Jun 19 2021 (Sat) literal");
        assert!(h("2021-06-18"), "Jun 18 2021 (observed Juneteenth)");
    }

    /// C4. `HolidayTest.kt:50-53`.
    #[test]
    fn a_plain_workday_is_not_a_holiday() {
        assert!(!h("2026-07-02"), "Jul 2 2026 (Thu)");
    }

    /// C4. `HolidayTest.kt:55-60`.
    #[test]
    fn floating_holidays_match_without_shifting() {
        // Thanksgiving 2026 = 4th Thursday of November = Nov 26.
        assert!(h("2026-11-26"), "Thanksgiving 2026");
        assert!(
            !h("2026-11-27"),
            "the Friday after Thanksgiving is not a federal holiday"
        );
    }

    // ---------------------------------------------------------------------
    // Each fixed holiday in each of the three week positions.
    // Weekday assignments measured on a JVM
    // (`~/.cache/tt-devpro-rewrite/measurements/holidays/Oracle.java`), not derived.
    // ---------------------------------------------------------------------

    /// C4, `Holidays.kt:38,44`. New Year on a weekday, a Saturday and a Sunday.
    /// The Saturday case is the cross-year one: the observed day belongs to the
    /// *previous* calendar year, which is why the fixed list carries `year + 1`.
    #[test]
    fn new_year_matches_literally_and_on_its_observed_day_in_every_week_position() {
        // Jan 1 2025 is a Wednesday — no shift, neighbours are workdays.
        assert_holidays(&["2025-01-01"]);
        assert_workdays(&["2024-12-31", "2025-01-02"]);

        // Jan 1 2022 is a Saturday → observed Fri Dec 31 2021.
        assert_holidays(&["2022-01-01", "2021-12-31"]);
        assert_workdays(&["2021-12-30", "2022-01-03"]);

        // Jan 1 2023 is a Sunday → observed Mon Jan 2 2023.
        assert_holidays(&["2023-01-01", "2023-01-02"]);
        assert_workdays(&["2022-12-30", "2023-01-03"]);
    }

    /// C4, `Holidays.kt:40`. Juneteenth in all three week positions.
    #[test]
    fn juneteenth_matches_literally_and_on_its_observed_day_in_every_week_position() {
        // Jun 19 2026 is a Friday.
        assert_holidays(&["2026-06-19"]);
        assert_workdays(&["2026-06-18", "2026-06-22"]);

        // Jun 19 2021 is a Saturday → observed Fri Jun 18.
        assert_holidays(&["2021-06-19", "2021-06-18"]);
        assert_workdays(&["2021-06-17", "2021-06-21"]);

        // Jun 19 2022 is a Sunday → observed Mon Jun 20.
        assert_holidays(&["2022-06-19", "2022-06-20"]);
        assert_workdays(&["2022-06-17", "2022-06-21"]);
    }

    /// C4, `Holidays.kt:41`. Independence Day in all three week positions.
    #[test]
    fn independence_day_matches_literally_and_on_its_observed_day_in_every_week_position() {
        // Jul 4 2025 is a Friday.
        assert_holidays(&["2025-07-04"]);
        assert_workdays(&["2025-07-03", "2025-07-07"]);

        // Jul 4 2020 is a Saturday → observed Fri Jul 3.
        assert_holidays(&["2020-07-04", "2020-07-03"]);
        assert_workdays(&["2020-07-02", "2020-07-06"]);

        // Jul 4 2021 is a Sunday → observed Mon Jul 5.
        assert_holidays(&["2021-07-04", "2021-07-05"]);
        assert_workdays(&["2021-07-02", "2021-07-06"]);
    }

    /// C4, `Holidays.kt:42`. Christmas in all three week positions.
    #[test]
    fn christmas_matches_literally_and_on_its_observed_day_in_every_week_position() {
        // Dec 25 2025 is a Thursday.
        assert_holidays(&["2025-12-25"]);
        assert_workdays(&["2025-12-24", "2025-12-26"]);

        // Dec 25 2021 is a Saturday → observed Fri Dec 24.
        assert_holidays(&["2021-12-25", "2021-12-24"]);
        assert_workdays(&["2021-12-23", "2021-12-27"]);

        // Dec 25 2022 is a Sunday → observed Mon Dec 26.
        assert_holidays(&["2022-12-25", "2022-12-26"]);
        assert_workdays(&["2022-12-23", "2022-12-27"]);
    }

    // ---------------------------------------------------------------------
    // The four floating holidays, over enough years to catch off-by-one in the
    // hand-written adjusters. Dates measured on a JVM
    // (`~/.cache/tt-devpro-rewrite/measurements/holidays/Oracle.java`).
    // ---------------------------------------------------------------------

    /// C4, `Holidays.kt:47-48`. MLK Jr. Day = 3rd Monday of January. The span of
    /// possible dates is Jan 15..Jan 21 and both ends are here: 2024 and 2029 hit
    /// the 15th (Jan 1 is itself a Monday), 2019 and 2030 the 21st.
    #[test]
    fn mlk_day_is_the_third_monday_of_january_in_every_year_tested() {
        assert_holidays(&[
            "2019-01-21",
            "2020-01-20",
            "2021-01-18",
            "2022-01-17",
            "2023-01-16",
            "2024-01-15",
            "2025-01-20",
            "2026-01-19",
            "2027-01-18",
            "2029-01-15",
            "2030-01-21",
        ]);
        // The second and fourth Mondays are ordinary workdays.
        assert_workdays(&["2026-01-12", "2026-01-26", "2024-01-08", "2024-01-22"]);
    }

    /// C4, `Holidays.kt:49-50`. Memorial Day = **last** Monday of May, which is a
    /// different adjuster from the other three. 2021, 2027 and 2032 are the years
    /// where it lands on May 31, the last day of the month — the case that
    /// separates "last Monday" from "4th Monday".
    #[test]
    fn memorial_day_is_the_last_monday_of_may_including_the_years_it_falls_on_may_31() {
        assert_holidays(&[
            "2020-05-25",
            "2021-05-31",
            "2022-05-30",
            "2023-05-29",
            "2024-05-27",
            "2025-05-26",
            "2026-05-25",
            "2027-05-31",
            "2032-05-31",
        ]);
        // 2021 and 2027 have five Mondays in May; the 4th is not the holiday.
        assert_workdays(&["2021-05-24", "2027-05-24", "2032-05-24"]);
    }

    /// C4, `Holidays.kt:51-52`. Labor Day = 1st Monday of September. 2025 and
    /// 2031 are the years where Sep 1 is itself a Monday — the case that breaks
    /// an adjuster written as "advance at least one day to the next Monday".
    #[test]
    fn labor_day_is_the_first_monday_of_september_including_the_years_it_falls_on_sep_1() {
        assert_holidays(&[
            "2020-09-07",
            "2021-09-06",
            "2022-09-05",
            "2023-09-04",
            "2024-09-02",
            "2025-09-01",
            "2026-09-07",
            "2031-09-01",
        ]);
        // The second Monday is an ordinary workday.
        assert_workdays(&["2025-09-08", "2026-09-14", "2031-09-08"]);
    }

    /// C4, `Holidays.kt:53-54`. Thanksgiving = 4th Thursday of November, spanning
    /// Nov 22..Nov 28; both ends are here (2029 the 22nd, 2019/2024/2030 the
    /// 28th). In the Nov-22 years November has five Thursdays, so "last Thursday"
    /// and "4th Thursday" disagree — `2029-11-29` pins which one this is.
    #[test]
    fn thanksgiving_is_the_fourth_thursday_of_november_not_the_last_one() {
        assert_holidays(&[
            "2019-11-28",
            "2020-11-26",
            "2021-11-25",
            "2022-11-24",
            "2023-11-23",
            "2024-11-28",
            "2025-11-27",
            "2026-11-26",
            "2029-11-22",
            "2030-11-28",
        ]);
        // 2029 has a fifth Thursday on Nov 29 — not the holiday.
        assert_workdays(&["2029-11-29", "2018-11-29"]);
    }

    /// C4, `Holidays.kt:46`. A floating holiday never gets the observance shift,
    /// and the weekend it abuts stays a plain weekend. Labor Day 2025 falls on
    /// Sep 1 with Aug 31 a Sunday, and Memorial Day 2021 on May 31 with May 30 a
    /// Sunday: an implementation that ran `observed()` over the floating list in
    /// the wrong direction would light those up.
    #[test]
    fn a_floating_holiday_never_shifts_onto_the_weekend_beside_it() {
        assert_holidays(&["2025-09-01", "2021-05-31"]);
        assert_workdays(&["2025-08-31", "2025-08-30", "2021-05-30", "2021-05-29"]);
    }

    // ---------------------------------------------------------------------
    // Year boundary, both directions.
    // ---------------------------------------------------------------------

    /// C4, `Holidays.kt:38`. Dec 31 is a holiday **only** when the following
    /// Jan 1 is a Saturday. This is the whole reason the fixed list carries
    /// `LocalDate.of(year + 1, JANUARY, 1)`; dropping that entry leaves every
    /// other case in this file passing.
    #[test]
    fn dec_31_is_a_holiday_only_when_the_next_jan_1_falls_on_a_saturday() {
        // Jan 1 2022 and Jan 1 2028 are Saturdays.
        assert_holidays(&["2021-12-31", "2027-12-31"]);
        // Jan 1 2021 Fri, Jan 1 2023 Sun, Jan 1 2026 Thu — no backward shift.
        assert_workdays(&["2020-12-31", "2022-12-31", "2025-12-31"]);
    }

    /// C4, `Holidays.kt:37`. The forward half of the same boundary: Jan 2 is a
    /// holiday only when Jan 1 is a Sunday, and it is found through the
    /// *current* year's entry, not the `year + 1` one.
    #[test]
    fn jan_2_is_a_holiday_only_when_jan_1_falls_on_a_sunday() {
        // Jan 1 2023 and Jan 1 2034 are Sundays.
        assert_holidays(&["2023-01-02", "2034-01-02"]);
        // Jan 1 2025 Wed, Jan 1 2026 Thu, Jan 1 2022 Sat (shifts backwards).
        assert_workdays(&["2025-01-02", "2026-01-02", "2022-01-02"]);
    }

    // ---------------------------------------------------------------------
    // Leap years, ordinary weeks, extremes.
    // ---------------------------------------------------------------------

    /// C4. A leap day is not a holiday, and the extra day does not move the
    /// floating holidays that follow it in the same year.
    #[test]
    fn a_leap_day_is_not_a_holiday_and_does_not_displace_the_rest_of_the_year() {
        assert_workdays(&["2020-02-29", "2024-02-29", "2028-02-29"]);
        assert_holidays(&["2024-05-27", "2024-09-02", "2024-11-28", "2024-12-25"]);
        assert_holidays(&["2020-05-25", "2020-09-07", "2020-11-26", "2020-12-25"]);
    }

    /// C4. A whole ordinary working week matches nothing. Sep 14-18 2026 is the
    /// week the parity capture was taken in.
    #[test]
    fn an_ordinary_working_week_contains_no_holiday() {
        assert_workdays(&[
            "2026-09-14",
            "2026-09-15",
            "2026-09-16",
            "2026-09-17",
            "2026-09-18",
            "2026-09-19",
            "2026-09-20",
        ]);
    }

    /// C4. The predicate is total: `chrono`'s calendar is bounded where
    /// `java.time`'s is not, so the `year + 1` construction and the observance
    /// shift both have to survive the ends of it rather than panic.
    #[test]
    fn the_predicate_does_not_panic_at_the_ends_of_the_calendar() {
        for date in [
            NaiveDate::MAX,
            NaiveDate::MAX.pred_opt().expect("max - 1"),
            NaiveDate::MIN,
            NaiveDate::MIN.succ_opt().expect("min + 1"),
        ] {
            let _ = is_us_federal_holiday(date);
        }
    }

    // ---------------------------------------------------------------------
    // The eight-holiday list itself.
    // ---------------------------------------------------------------------

    /// C4 — the deliberate omission, stated as a test so that "finishing the US
    /// federal calendar" cannot land quietly. Presidents' Day, Columbus Day and
    /// Veterans Day are real federal holidays and `Holidays.kt` does not list
    /// them. Adding them would stop `settle` offering three real workdays a year.
    /// The Veterans Day rows include the observance candidates it *would* acquire
    /// (Nov 11 2023 is a Saturday, Nov 11 2029 a Sunday), because a port that
    /// added it as a fixed-date holiday would light those up too.
    #[test]
    fn presidents_day_columbus_day_and_veterans_day_are_deliberately_absent() {
        assert_workdays(&[
            // Presidents' Day — 3rd Monday of February.
            "2023-02-20",
            "2024-02-19",
            "2025-02-17",
            "2026-02-16",
            // Columbus Day — 2nd Monday of October.
            "2023-10-09",
            "2024-10-14",
            "2025-10-13",
            "2026-10-12",
            // Veterans Day — Nov 11, literal.
            "2024-11-11",
            "2025-11-11",
            "2026-11-11",
            "2023-11-11",
            "2029-11-11",
            // ...and the two days it would be observed on if it were listed.
            "2023-11-10",
            "2029-11-12",
        ]);
    }

    /// Every day the predicate matches, per calendar year, measured by running
    /// `Holidays.kt`'s exact logic on a JVM (`java.time`, JDK 21 semantics) and
    /// sweeping each year day by day. Eight base holidays plus whatever
    /// observance shifts that year produces: 2021, 2027 and 2032 reach twelve,
    /// 2024 and 2025 stay at eight.
    const JVM_YEAR_SETS: &[(i32, &[&str])] = &[
        (
            2018,
            &[
                "2018-01-01",
                "2018-01-15",
                "2018-05-28",
                "2018-06-19",
                "2018-07-04",
                "2018-09-03",
                "2018-11-22",
                "2018-12-25",
            ],
        ),
        (
            2019,
            &[
                "2019-01-01",
                "2019-01-21",
                "2019-05-27",
                "2019-06-19",
                "2019-07-04",
                "2019-09-02",
                "2019-11-28",
                "2019-12-25",
            ],
        ),
        (
            2020,
            &[
                "2020-01-01",
                "2020-01-20",
                "2020-05-25",
                "2020-06-19",
                "2020-07-03",
                "2020-07-04",
                "2020-09-07",
                "2020-11-26",
                "2020-12-25",
            ],
        ),
        (
            2021,
            &[
                "2021-01-01",
                "2021-01-18",
                "2021-05-31",
                "2021-06-18",
                "2021-06-19",
                "2021-07-04",
                "2021-07-05",
                "2021-09-06",
                "2021-11-25",
                "2021-12-24",
                "2021-12-25",
                "2021-12-31",
            ],
        ),
        (
            2022,
            &[
                "2022-01-01",
                "2022-01-17",
                "2022-05-30",
                "2022-06-19",
                "2022-06-20",
                "2022-07-04",
                "2022-09-05",
                "2022-11-24",
                "2022-12-25",
                "2022-12-26",
            ],
        ),
        (
            2023,
            &[
                "2023-01-01",
                "2023-01-02",
                "2023-01-16",
                "2023-05-29",
                "2023-06-19",
                "2023-07-04",
                "2023-09-04",
                "2023-11-23",
                "2023-12-25",
            ],
        ),
        (
            2024,
            &[
                "2024-01-01",
                "2024-01-15",
                "2024-05-27",
                "2024-06-19",
                "2024-07-04",
                "2024-09-02",
                "2024-11-28",
                "2024-12-25",
            ],
        ),
        (
            2025,
            &[
                "2025-01-01",
                "2025-01-20",
                "2025-05-26",
                "2025-06-19",
                "2025-07-04",
                "2025-09-01",
                "2025-11-27",
                "2025-12-25",
            ],
        ),
        (
            2026,
            &[
                "2026-01-01",
                "2026-01-19",
                "2026-05-25",
                "2026-06-19",
                "2026-07-03",
                "2026-07-04",
                "2026-09-07",
                "2026-11-26",
                "2026-12-25",
            ],
        ),
        (
            2027,
            &[
                "2027-01-01",
                "2027-01-18",
                "2027-05-31",
                "2027-06-18",
                "2027-06-19",
                "2027-07-04",
                "2027-07-05",
                "2027-09-06",
                "2027-11-25",
                "2027-12-24",
                "2027-12-25",
                "2027-12-31",
            ],
        ),
        (
            2028,
            &[
                "2028-01-01",
                "2028-01-17",
                "2028-05-29",
                "2028-06-19",
                "2028-07-04",
                "2028-09-04",
                "2028-11-23",
                "2028-12-25",
            ],
        ),
        (
            2029,
            &[
                "2029-01-01",
                "2029-01-15",
                "2029-05-28",
                "2029-06-19",
                "2029-07-04",
                "2029-09-03",
                "2029-11-22",
                "2029-12-25",
            ],
        ),
        (
            2030,
            &[
                "2030-01-01",
                "2030-01-21",
                "2030-05-27",
                "2030-06-19",
                "2030-07-04",
                "2030-09-02",
                "2030-11-28",
                "2030-12-25",
            ],
        ),
        (
            2031,
            &[
                "2031-01-01",
                "2031-01-20",
                "2031-05-26",
                "2031-06-19",
                "2031-07-04",
                "2031-09-01",
                "2031-11-27",
                "2031-12-25",
            ],
        ),
        (
            2032,
            &[
                "2032-01-01",
                "2032-01-19",
                "2032-05-31",
                "2032-06-18",
                "2032-06-19",
                "2032-07-04",
                "2032-07-05",
                "2032-09-06",
                "2032-11-25",
                "2032-12-24",
                "2032-12-25",
                "2032-12-31",
            ],
        ),
        (
            2033,
            &[
                "2033-01-01",
                "2033-01-17",
                "2033-05-30",
                "2033-06-19",
                "2033-06-20",
                "2033-07-04",
                "2033-09-05",
                "2033-11-24",
                "2033-12-25",
                "2033-12-26",
            ],
        ),
        (
            2034,
            &[
                "2034-01-01",
                "2034-01-02",
                "2034-01-16",
                "2034-05-29",
                "2034-06-19",
                "2034-07-04",
                "2034-09-04",
                "2034-11-23",
                "2034-12-25",
            ],
        ),
        (
            2035,
            &[
                "2035-01-01",
                "2035-01-15",
                "2035-05-28",
                "2035-06-19",
                "2035-07-04",
                "2035-09-03",
                "2035-11-22",
                "2035-12-25",
            ],
        ),
    ];

    /// C4, the whole of `Holidays.kt` at once. Sweeping all 6 574 days of
    /// 2018-2035 and comparing the matched set against the JVM's is the test that
    /// closes the gap the case-by-case ones leave: a spurious extra match on some
    /// day nobody thought to name fails here and nowhere else. It also pins the
    /// *count* per year, so an eleventh holiday cannot be smuggled in.
    #[test]
    fn a_full_year_sweep_matches_the_jvm_day_for_day_from_2018_to_2035() {
        for (year, expected) in JVM_YEAR_SETS {
            let mut matched = Vec::new();
            let mut day = NaiveDate::from_ymd_opt(*year, 1, 1).expect("Jan 1");
            while day.year() == *year {
                if is_us_federal_holiday(day) {
                    matched.push(day.to_string());
                }
                day = day.succ_opt().expect("next day");
            }
            assert_eq!(matched, *expected, "matched holidays of {year}");
        }
    }

    /// C4, `Holidays.kt:44`. The days that match **only** through the observance
    /// shift, isolated from the literal dates. Derived from `JVM_YEAR_SETS` minus
    /// the eight literal holidays of each year: an implementation that dropped
    /// `observed()` entirely would still pass every literal-date assertion in
    /// this file, and would fail exactly here.
    #[test]
    fn the_observance_shift_is_what_adds_these_days_and_nothing_else_adds_them() {
        assert_holidays(&[
            "2020-07-03", // Jul 4 2020 Sat
            "2021-06-18", // Jun 19 2021 Sat
            "2021-07-05", // Jul 4 2021 Sun
            "2021-12-24", // Dec 25 2021 Sat
            "2021-12-31", // Jan 1 2022 Sat
            "2022-06-20", // Jun 19 2022 Sun
            "2022-12-26", // Dec 25 2022 Sun
            "2023-01-02", // Jan 1 2023 Sun
            "2026-07-03", // Jul 4 2026 Sat
            "2027-06-18", // Jun 19 2027 Sat
            "2027-07-05", // Jul 4 2027 Sun
            "2027-12-24", // Dec 25 2027 Sat
            "2027-12-31", // Jan 1 2028 Sat
        ]);
    }

    // ---------------------------------------------------------------------
    // The two hand-written adjusters, against JVM measurements
    // (`~/.cache/tt-devpro-rewrite/measurements/holidays/Oracle.java`).
    // ---------------------------------------------------------------------

    /// `observed` (`Holidays.kt:23-27`) shifts on exactly two of the seven
    /// weekdays and leaves the other five alone. The week of Sep 14 2026 is a
    /// Monday-to-Sunday run, so this walks every branch.
    #[test]
    fn observed_shifts_saturday_back_and_sunday_forward_and_nothing_else() {
        assert_eq!(observed(d("2026-09-14")), d("2026-09-14"), "Mon");
        assert_eq!(observed(d("2026-09-15")), d("2026-09-15"), "Tue");
        assert_eq!(observed(d("2026-09-16")), d("2026-09-16"), "Wed");
        assert_eq!(observed(d("2026-09-17")), d("2026-09-17"), "Thu");
        assert_eq!(observed(d("2026-09-18")), d("2026-09-18"), "Fri");
        assert_eq!(observed(d("2026-09-19")), d("2026-09-18"), "Sat → Fri");
        assert_eq!(observed(d("2026-09-20")), d("2026-09-21"), "Sun → Mon");
    }

    /// `observed` crosses a month and a year boundary rather than clamping to the
    /// month it started in. Jan 1 2022 is the case `settle` actually depends on.
    #[test]
    fn observed_crosses_month_and_year_boundaries() {
        assert_eq!(
            observed(d("2022-01-01")),
            d("2021-12-31"),
            "Sat → prior year"
        );
        assert_eq!(
            observed(d("2021-10-31")),
            d("2021-11-01"),
            "Sun → next month"
        );
        assert_eq!(
            observed(d("2021-05-01")),
            d("2021-04-30"),
            "Sat → prior month"
        );
    }

    /// `nth_weekday_in_month` against `TemporalAdjusters.dayOfWeekInMonth`,
    /// measured on a JVM. The last row is the roll-over: February 2026 has four
    /// Mondays, and Java's 5th is `2026-03-02`, not a clamp to the 23rd.
    #[test]
    fn nth_weekday_in_month_reproduces_the_jvm_adjuster_including_its_roll_over() {
        let cases: &[(i32, u32, Weekday, u32, &str)] = &[
            (2026, 1, Weekday::Mon, 3, "2026-01-19"),
            (2019, 1, Weekday::Mon, 3, "2019-01-21"),
            (2024, 1, Weekday::Mon, 3, "2024-01-15"),
            (2025, 9, Weekday::Mon, 1, "2025-09-01"),
            (2026, 9, Weekday::Mon, 1, "2026-09-07"),
            (2026, 11, Weekday::Thu, 4, "2026-11-26"),
            (2029, 11, Weekday::Thu, 4, "2029-11-22"),
            (2026, 2, Weekday::Mon, 1, "2026-02-02"),
            (2026, 2, Weekday::Mon, 5, "2026-03-02"),
        ];
        for (year, month, weekday, n, expected) in cases {
            assert_eq!(
                nth_weekday_in_month(*year, *month, *weekday, *n),
                Some(d(expected)),
                "{n} x {weekday:?} in {year}-{month:02}"
            );
        }
    }

    /// `last_weekday_in_month` against `TemporalAdjusters.lastInMonth`, measured
    /// on a JVM. February 2024's last Thursday is the 29th, which is the row that
    /// separates a correct last-day-of-month from a hardcoded 28.
    #[test]
    fn last_weekday_in_month_reproduces_the_jvm_adjuster_across_month_lengths() {
        let cases: &[(i32, u32, Weekday, &str)] = &[
            (2021, 5, Weekday::Mon, "2021-05-31"),
            (2026, 5, Weekday::Mon, "2026-05-25"),
            (2024, 2, Weekday::Mon, "2024-02-26"),
            (2024, 2, Weekday::Thu, "2024-02-29"),
            (2026, 12, Weekday::Mon, "2026-12-28"),
        ];
        for (year, month, weekday, expected) in cases {
            assert_eq!(
                last_weekday_in_month(*year, *month, *weekday),
                Some(d(expected)),
                "last {weekday:?} in {year}-{month:02}"
            );
        }
    }

    /// The two adjusters disagree in every May where the month holds five
    /// Mondays, which is why Memorial Day uses `lastInMonth` and not
    /// `dayOfWeekInMonth(4, MONDAY)`. A port that unified the two call shapes
    /// would move Memorial Day a week earlier in 2021, 2027 and 2032.
    #[test]
    fn the_last_monday_of_may_differs_from_the_fourth_one_in_five_monday_years() {
        for year in [2021, 2027, 2032] {
            let fourth = nth_weekday_in_month(year, 5, Weekday::Mon, 4).expect("4th Mon");
            let last = last_weekday_in_month(year, 5, Weekday::Mon).expect("last Mon");
            assert_ne!(fourth, last, "May {year} holds five Mondays");
            assert!(is_us_federal_holiday(last), "last Monday is Memorial Day");
            assert!(!is_us_federal_holiday(fourth), "the 4th Monday is not");
        }
        // ...and they coincide in a four-Monday May, so the test above is about
        // the adjuster and not about May in general.
        for year in [2024, 2025, 2026] {
            assert_eq!(
                nth_weekday_in_month(year, 5, Weekday::Mon, 4),
                last_weekday_in_month(year, 5, Weekday::Mon),
                "May {year} holds four Mondays"
            );
        }
    }
}
