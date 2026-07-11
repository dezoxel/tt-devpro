package pro.dev.tt

import pro.dev.tt.commands.ActionType
import pro.dev.tt.commands.SettleAction
import pro.dev.tt.commands.cleanChronoEntry
import pro.dev.tt.commands.entryType
import pro.dev.tt.commands.renderDaySummary
import pro.dev.tt.service.DayProjectAggregate
import java.time.LocalDate
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class DaySummaryTest {

    private fun action(
        date: String,
        devproProject: String,
        hours: Double,
        title: String,
        isMeeting: Boolean = false,
        isFiller: Boolean = false,
        isBorrowed: Boolean = false,
        chronoProject: String = "Some Chrono Project",
        descriptions: List<String> = listOf(title),
        action: ActionType = ActionType.CREATE
    ) = SettleAction(
        aggregate = DayProjectAggregate(
            date = LocalDate.parse(date),
            chronoProject = chronoProject,
            totalHours = hours,
            descriptions = descriptions,
            devproProjectName = devproProject,
            billability = "Billable"
        ),
        normalizedHours = hours,
        isMeeting = isMeeting,
        isFiller = isFiller,
        isBorrowed = isBorrowed,
        taskTitle = title,
        devproProjectId = "id-$devproProject",
        action = action
    )

    @Test
    fun `empty input yields a friendly message`() {
        assertEquals("No actions to settle.", renderDaySummary(emptyList()))
    }

    @Test
    fun `groups by day with per-day totals and weekday`() {
        val out = renderDaySummary(
            listOf(
                action("2026-07-10", "Velocitor: NLP", 6.0, "Sprint work"),
                action("2026-07-10", "AI Practice", 2.0, "AI Team Sync", isMeeting = true),
                action("2026-07-09", "Presales", 8.0, "Discovery"),
            )
        )
        // 2026-07-10 is a Friday, 2026-07-09 a Thursday
        assertTrue(out.contains("2026-07-10 Fri — 2 entries → 8.00h"), out)
        assertTrue(out.contains("2026-07-09 Thu — 1 entry → 8.00h"), out)
        // Days are sorted ascending (Thu block before Fri block)
        assertTrue(out.indexOf("2026-07-09") < out.indexOf("2026-07-10"), out)
    }

    @Test
    fun `renders type, hours, action and quoted title per row`() {
        val out = renderDaySummary(listOf(action("2026-07-10", "Velocitor: NLP", 6.0, "Sprint work")))
        assertTrue(out.contains("Velocitor: NLP"), out)
        assertTrue(out.contains("Work"), out)
        assertTrue(out.contains("6.00"), out)
        assertTrue(out.contains("Create"), out)
        assertTrue(out.contains("\"Sprint work\""), out)
    }

    @Test
    fun `grand total sums hours across all days and entries`() {
        val out = renderDaySummary(
            listOf(
                action("2026-07-10", "A", 6.0, "x"),
                action("2026-07-10", "B", 2.0, "y"),
                action("2026-07-09", "A", 8.0, "z"),
            )
        )
        assertTrue(out.contains("Total: 16.00h across 2 days, 3 entries"), out)
    }

    @Test
    fun `entryType reflects meeting flag`() {
        assertEquals("Meeting", entryType(action("2026-07-10", "A", 1.0, "sync", isMeeting = true)))
        assertEquals("Work", entryType(action("2026-07-10", "A", 1.0, "code")))
    }

    @Test
    fun `cleanChronoEntry strips project suffix and marks synthetic entries`() {
        val normal = action(
            "2026-07-10", "A", 1.0, "Wrote docs",
            chronoProject = "MyProj",
            descriptions = listOf("Wrote docs - MyProj")
        )
        assertEquals("Wrote docs", cleanChronoEntry(normal))
        assertEquals("[filler]", cleanChronoEntry(action("2026-07-10", "A", 1.0, "f", isFiller = true)))
        assertEquals("[borrowed]", cleanChronoEntry(action("2026-07-10", "A", 1.0, "b", isBorrowed = true)))
    }
}
