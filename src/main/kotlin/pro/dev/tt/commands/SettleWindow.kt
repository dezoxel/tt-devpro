package pro.dev.tt.commands

import java.time.LocalDate

/**
 * Pure day-boundary rules for `settle`, extracted so they can be unit-tested
 * without a live CLI. Nothing here does I/O.
 *
 * `settle` proposes hours for days that came back under 8h. Today always
 * qualifies — it is unfinished *by construction*, so it matched that predicate
 * every single morning and the filler/borrowing synthesis rounded a
 * half-finished day up to a plausible-looking 8h, sitting in the review table
 * right next to the legitimate previous day. One inattentive `[A]` published
 * that fabrication to the DevPro time report.
 *
 * Future days got in the same way: Chrono holds planned/calendar entries for
 * days that have not started, and the candidate scan had no upper bound at all.
 *
 * The rule is therefore a hard boundary, not a heuristic: the window ends at
 * the last *completed* day. Deliberately not "skip today if it looks thin" —
 * today is unfinished no matter how full it looks, and a threshold would let a
 * busy morning slip through.
 */

/**
 * The last date `settle` may propose. Yesterday by default; [includeToday]
 * moves it to today for the rare deliberate case (closing the books early
 * before time off) — never past it, since a future day has no hours at all.
 */
internal fun lastSettleableDay(today: LocalDate, includeToday: Boolean): LocalDate =
    if (includeToday) today else today.minusDays(1)

/** Candidate days split by whether their hours are final. */
internal data class DayWindow(
    val settleable: List<LocalDate>,
    val notFinal: List<LocalDate>
)

/**
 * Splits [candidates] at the cutoff, keeping both halves: the caller says out
 * loud what it dropped rather than silently shrinking the list, because a day
 * vanishing without explanation is what sent the last investigation looking in
 * the wrong place.
 */
internal fun splitByFinality(
    candidates: List<LocalDate>,
    today: LocalDate,
    includeToday: Boolean
): DayWindow {
    val settleThrough = lastSettleableDay(today, includeToday)
    val (settleable, notFinal) = candidates.partition { !it.isAfter(settleThrough) }
    return DayWindow(settleable, notFinal)
}

/** Renders a not-final day, marking today so the reason is obvious. */
internal fun describeNotFinal(day: LocalDate, today: LocalDate): String =
    if (day == today) "$day (today)" else "$day"

/**
 * Names the days held back, suggesting `--include-today` only when today is
 * actually among them. Offering the flag for a day that no flag can unlock —
 * tomorrow, under `--include-today` — is advice that cannot be followed.
 */
internal fun describeNotFinalDays(notFinal: List<LocalDate>, today: LocalDate): String {
    val listed = notFinal.joinToString(", ") { describeNotFinal(it, today) }
    return if (notFinal.contains(today)) {
        "$listed. Use --include-today to settle today anyway."
    } else {
        "$listed — those days haven't happened yet."
    }
}

/**
 * The empty-scan message. "All days are settled" is only true when nothing was
 * held back — saying it while the skip notice reports a dropped day gives two
 * contradictory answers to the same question on two different streams.
 */
internal fun nothingToSettleMessage(notFinal: List<LocalDate>, today: LocalDate): String =
    if (notFinal.isEmpty()) {
        "All days are settled (≥8h logged)."
    } else {
        "Nothing to settle yet. Held back: ${describeNotFinalDays(notFinal, today)}"
    }
