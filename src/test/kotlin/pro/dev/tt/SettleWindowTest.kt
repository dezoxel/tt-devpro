package pro.dev.tt

import pro.dev.tt.commands.describeNotFinalDays
import pro.dev.tt.commands.lastSettleableDay
import pro.dev.tt.commands.nothingToSettleMessage
import pro.dev.tt.commands.splitByFinality
import java.time.LocalDate
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class SettleWindowTest {

    private val today = LocalDate.parse("2026-08-13")
    private val yesterday = LocalDate.parse("2026-08-12")
    private val tomorrow = LocalDate.parse("2026-08-14")

    @Test
    fun `the default window ends at yesterday`() {
        assertEquals(yesterday, lastSettleableDay(today, includeToday = false))
    }

    @Test
    fun `include-today moves the cutoff to today and no further`() {
        assertEquals(today, lastSettleableDay(today, includeToday = true))
    }

    @Test
    fun `today is dropped by default`() {
        val window = splitByFinality(listOf(yesterday, today), today, includeToday = false)
        assertEquals(listOf(yesterday), window.settleable)
        assertEquals(listOf(today), window.notFinal)
    }

    @Test
    fun `today is kept with include-today`() {
        val window = splitByFinality(listOf(yesterday, today), today, includeToday = true)
        assertEquals(listOf(yesterday, today), window.settleable)
        assertTrue(window.notFinal.isEmpty(), "nothing is held back once today is allowed")
    }

    @Test
    fun `a future day is dropped even with include-today`() {
        // Chrono holds planned entries for days that haven't started; no flag
        // should ever make those settleable.
        val window = splitByFinality(listOf(today, tomorrow), today, includeToday = true)
        assertEquals(listOf(today), window.settleable)
        assertEquals(listOf(tomorrow), window.notFinal)
    }

    @Test
    fun `past days are always kept`() {
        val past = listOf("2026-08-10", "2026-08-11", "2026-08-12").map(LocalDate::parse)
        val window = splitByFinality(past, today, includeToday = false)
        assertEquals(past, window.settleable)
        assertTrue(window.notFinal.isEmpty())
    }

    @Test
    fun `notFinal is empty when every candidate is in the past`() {
        // This emptiness is what decides between "all days are settled" and
        // "nothing final yet" in the caller's empty-state message.
        val window = splitByFinality(listOf(yesterday), today, includeToday = false)
        assertTrue(window.notFinal.isEmpty())
    }

    @Test
    fun `everything past the cutoff is reported, not just today`() {
        val window = splitByFinality(listOf(yesterday, today, tomorrow), today, includeToday = false)
        assertEquals(listOf(yesterday), window.settleable)
        assertEquals(listOf(today, tomorrow), window.notFinal)
    }

    @Test
    fun `the include-today hint appears only when today was held back`() {
        val withToday = describeNotFinalDays(listOf(today), today)
        assertTrue(withToday.contains("2026-08-13 (today)"))
        assertTrue(withToday.contains("--include-today"), "the flag can actually unlock today")
    }

    @Test
    fun `a future-only holdback does not suggest a flag that cannot help`() {
        // Reachable under --include-today: today is settleable, tomorrow still
        // isn't, and telling the user to pass the flag they already passed is
        // advice that cannot be followed.
        val futureOnly = describeNotFinalDays(listOf(tomorrow), today)
        assertTrue(futureOnly.contains("2026-08-14"))
        assertFalse(futureOnly.contains("--include-today"), "no flag makes a future day settleable")
    }

    @Test
    fun `an empty scan claims everything is settled only when nothing was held back`() {
        assertEquals(
            "All days are settled (≥8h logged).",
            nothingToSettleMessage(emptyList(), today)
        )
    }

    @Test
    fun `an empty scan that held today back does not claim everything is settled`() {
        // The contradiction this guards against: stdout saying "all settled"
        // while stderr says a day was skipped.
        val message = nothingToSettleMessage(listOf(today), today)
        assertFalse(message.contains("All days are settled"), "nothing was settled — today was held back")
        assertTrue(message.contains("2026-08-13 (today)"))
        assertTrue(message.contains("--include-today"))
    }
}
