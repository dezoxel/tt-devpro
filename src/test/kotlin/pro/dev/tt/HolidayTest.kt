package pro.dev.tt

import pro.dev.tt.commands.isUsFederalHoliday
import java.time.LocalDate
import kotlin.test.Test
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class HolidayTest {

    private fun holiday(date: String) = isUsFederalHoliday(LocalDate.parse(date))

    @Test
    fun `Saturday holiday is observed the preceding Friday`() {
        // Jul 4 2026 is a Saturday → observed Friday Jul 3.
        assertTrue(holiday("2026-07-03"), "Jul 3 2026 (observed Independence Day)")
        assertTrue(holiday("2026-07-04"), "Jul 4 2026 (literal Independence Day)")
    }

    @Test
    fun `weekday holiday does not shift`() {
        // Jul 4 2025 is a Friday — the day itself, nothing adjacent.
        assertTrue(holiday("2025-07-04"), "Jul 4 2025 (Fri)")
        assertFalse(holiday("2025-07-03"), "Jul 3 2025 (Thu) is a normal workday")
    }

    @Test
    fun `Sunday holiday is observed the following Monday`() {
        // Dec 25 2022 is a Sunday → observed Monday Dec 26.
        assertTrue(holiday("2022-12-25"), "Dec 25 2022 (literal Christmas)")
        assertTrue(holiday("2022-12-26"), "Dec 26 2022 (observed Christmas)")
    }

    @Test
    fun `New Year on Saturday is observed on Dec 31 of the prior year`() {
        // Jan 1 2022 is a Saturday → observed Friday Dec 31 2021 (cross-year).
        assertTrue(holiday("2022-01-01"), "Jan 1 2022 (literal New Year)")
        assertTrue(holiday("2021-12-31"), "Dec 31 2021 (observed New Year)")
    }

    @Test
    fun `Juneteenth is a fixed holiday and shifts on weekends`() {
        assertTrue(holiday("2026-06-19"), "Jun 19 2026 (Fri) literal")
        // Jun 19 2021 is a Saturday → observed Friday Jun 18.
        assertTrue(holiday("2021-06-19"), "Jun 19 2021 (Sat) literal")
        assertTrue(holiday("2021-06-18"), "Jun 18 2021 (observed Juneteenth)")
    }

    @Test
    fun `a plain workday is not a holiday`() {
        assertFalse(holiday("2026-07-02"), "Jul 2 2026 (Thu)")
    }

    @Test
    fun `floating holidays match without shifting`() {
        // Thanksgiving 2026 = 4th Thursday of November = Nov 26.
        assertTrue(holiday("2026-11-26"), "Thanksgiving 2026")
        assertFalse(holiday("2026-11-27"), "the Friday after Thanksgiving is not a federal holiday")
    }
}
