package pro.dev.tt.service

import java.io.File
import java.time.LocalDate
import kotlin.math.roundToInt

/**
 * Normalizes daily hours to exactly 8h.
 * - Meetings are not adjusted (Operations projects + calendar events)
 * - Non-meeting entries are scaled proportionally
 * - All hours rounded to 0.25h increments
 */
object TimeNormalizer {
    private const val TARGET_HOURS = 8.0
    private const val HOUR_INCREMENT = 0.25
    private const val KNOWLEDGE_BASE = "/Users/iurii.buchchenko/knowledge-base"

    data class NormalizedAggregate(
        val original: DayProjectAggregate,
        val normalizedHours: Double,
        val isMeeting: Boolean
    )

    fun normalize(aggregates: List<DayProjectAggregate>): List<NormalizedAggregate> {
        // Group by date
        val byDate = aggregates.groupBy { it.date }

        return byDate.flatMap { (date, dayAggregates) ->
            normalizeDay(dayAggregates)
        }.sortedWith(compareBy({ it.original.date }, { it.original.devproProjectName }))
    }

    private fun normalizeDay(aggregates: List<DayProjectAggregate>): List<NormalizedAggregate> {
        // Determine which entries are meetings
        val withMeetingFlag = aggregates.map { agg ->
            val isMeeting = isMeetingEntry(agg)
            NormalizedAggregate(agg, agg.totalHours, isMeeting)
        }

        // Fixed entries: meetings OR capped (maxHours set) - these are not scaled
        val fixedEntries = withMeetingFlag.filter { it.isMeeting || it.original.maxHours != null }
        val fixedHours = fixedEntries.sumOf { it.normalizedHours }

        // Work entries: non-meetings without maxHours - these will be scaled
        val workEntries = withMeetingFlag.filter { !it.isMeeting && it.original.maxHours == null }
        val workHours = workEntries.sumOf { it.normalizedHours }
        val totalHours = fixedHours + workHours

        // If already 8h (within rounding), just round everything
        if (kotlin.math.abs(totalHours - TARGET_HOURS) < HOUR_INCREMENT / 2) {
            return withMeetingFlag.map { it.copy(normalizedHours = roundToQuarter(it.normalizedHours)) }
        }

        // Target hours for work entries (excluding fixed entries)
        val targetWorkHours = TARGET_HOURS - fixedHours

        // If no work entries or target is negative/zero, can't normalize
        if (workEntries.isEmpty() || targetWorkHours <= 0) {
            return withMeetingFlag.map { it.copy(normalizedHours = roundToQuarter(it.normalizedHours)) }
        }

        // Scale factor for work entries
        val scaleFactor = targetWorkHours / workHours

        // Apply proportional scaling and round
        val scaledWork = workEntries.map { entry ->
            val scaled = entry.normalizedHours * scaleFactor
            entry.copy(normalizedHours = roundToQuarter(scaled))
        }

        // After rounding, sum may not be exactly targetWorkHours
        // Adjust the largest entry to hit exactly target
        val scaledWorkTotal = scaledWork.sumOf { it.normalizedHours }
        val roundedFixed = fixedEntries
            .map { it.copy(normalizedHours = roundToQuarter(it.normalizedHours)) }
        val roundedFixedTotal = roundedFixed.sumOf { it.normalizedHours }

        val adjustedTarget = TARGET_HOURS - roundedFixedTotal
        val diff = adjustedTarget - scaledWorkTotal

        val finalWork = if (kotlin.math.abs(diff) >= HOUR_INCREMENT / 2 && scaledWork.isNotEmpty()) {
            // Find the largest scalable entry and adjust it
            val sorted = scaledWork.sortedByDescending { it.normalizedHours }
            val largest = sorted.first()
            val adjusted = largest.copy(normalizedHours = roundToQuarter(largest.normalizedHours + diff))
            listOf(adjusted) + sorted.drop(1)
        } else {
            scaledWork
        }

        return (roundedFixed + finalWork)
            .sortedWith(compareBy({ it.original.date }, { it.original.devproProjectName }))
    }

    private fun isMeetingEntry(agg: DayProjectAggregate): Boolean {
        // Operations projects are always "meetings" (admin work, not scaled)
        if (agg.chronoProject.startsWith("Operations -")) {
            return true
        }

        // Check if description refers to a calendar event (meeting note in Obsidian)
        if (agg.descriptions.isNotEmpty()) {
            val noteFile = File("$KNOWLEDGE_BASE/${agg.descriptions.first()}.md")
            if (noteFile.exists() && noteFile.readText().contains("The task represents the calendar event")) {
                return true
            }
        }

        return false
    }

    private fun roundToQuarter(hours: Double): Double {
        // Round to nearest 0.25
        return (hours / HOUR_INCREMENT).roundToInt() * HOUR_INCREMENT
    }
}
