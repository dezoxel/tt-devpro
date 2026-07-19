package pro.dev.tt.commands

import java.time.DayOfWeek
import java.time.LocalDate
import java.time.Month
import java.time.temporal.TemporalAdjusters

/**
 * Pure US federal holiday calendar predicate, extracted so it can be unit-tested
 * without a live CLI. Nothing here does I/O.
 *
 * Fixed-date holidays are *observed* on an adjacent weekday when they fall on a
 * weekend: a Saturday holiday is observed the preceding Friday, a Sunday holiday
 * the following Monday. `settle`'s "unfilled workday" scan must treat the
 * observed day as off too, otherwise it pads a real day-off with fabricated
 * hours (e.g. Jul 4 2026 is a Saturday → the day off is Friday Jul 3).
 *
 * Floating holidays (MLK, Memorial, Labor, Thanksgiving) are defined as an
 * "nth weekday of the month", so they can never land on a weekend and need no
 * shifting.
 */

/** Weekend-observance shift for a fixed-date holiday. */
private fun observed(actual: LocalDate): LocalDate = when (actual.dayOfWeek) {
    DayOfWeek.SATURDAY -> actual.minusDays(1)
    DayOfWeek.SUNDAY -> actual.plusDays(1)
    else -> actual
}

internal fun isUsFederalHoliday(date: LocalDate): Boolean {
    val year = date.year

    // Fixed-date holidays. Next year's New Year is included because a Jan 1 that
    // falls on a Saturday is observed on Dec 31 of the *current* year — the only
    // fixed holiday adjacent to a year boundary. A day matches if it is either
    // the literal date or its observed shift.
    val fixed = listOf(
        LocalDate.of(year, Month.JANUARY, 1),
        LocalDate.of(year + 1, Month.JANUARY, 1),
        LocalDate.of(year, Month.JUNE, 19),      // Juneteenth
        LocalDate.of(year, Month.JULY, 4),       // Independence Day
        LocalDate.of(year, Month.DECEMBER, 25),  // Christmas
    )
    if (fixed.any { date == it || date == observed(it) }) return true

    // Floating holidays — never a weekend, so no observed shift.
    val floating = listOf(
        LocalDate.of(year, Month.JANUARY, 1)
            .with(TemporalAdjusters.dayOfWeekInMonth(3, DayOfWeek.MONDAY)),   // MLK Jr. Day
        LocalDate.of(year, Month.MAY, 1)
            .with(TemporalAdjusters.lastInMonth(DayOfWeek.MONDAY)),          // Memorial Day
        LocalDate.of(year, Month.SEPTEMBER, 1)
            .with(TemporalAdjusters.dayOfWeekInMonth(1, DayOfWeek.MONDAY)),  // Labor Day
        LocalDate.of(year, Month.NOVEMBER, 1)
            .with(TemporalAdjusters.dayOfWeekInMonth(4, DayOfWeek.THURSDAY)),// Thanksgiving
    )
    return date in floating
}
