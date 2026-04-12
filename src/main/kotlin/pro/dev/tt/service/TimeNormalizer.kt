package pro.dev.tt.service

import java.nio.file.Files
import java.nio.file.Path
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

    private val calendarDirs: List<Path> by lazy {
        try {
            Files.walk(Path.of(KNOWLEDGE_BASE), 10)
                .filter { Files.isDirectory(it) && it.fileName.toString() == "Calendar" }
                .toList()
        } catch (_: Exception) {
            emptyList()
        }
    }

    private val sanitizeRegex = Regex("""[/\\:*?"<>|]""")
    private val dateSuffixRegex = Regex(""", (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d{1,2} \d{4}$""")

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

        // Apply proportional scaling and round (minimum 0.25h to avoid zero entries)
        val scaledWork = workEntries.map { entry ->
            val scaled = entry.normalizedHours * scaleFactor
            entry.copy(normalizedHours = maxOf(HOUR_INCREMENT, roundToQuarter(scaled)))
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
            // Find the largest scalable entry and adjust it (minimum 0.25h)
            val sorted = scaledWork.sortedByDescending { it.normalizedHours }
            val largest = sorted.first()
            val adjusted = largest.copy(normalizedHours = maxOf(HOUR_INCREMENT, roundToQuarter(largest.normalizedHours + diff)))
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

        if (agg.descriptions.isEmpty()) {
            return false
        }

        // Extract meeting name from description (strip project suffix + date)
        // Old format: "Event Name, Apr 8 2026 - Project - DevPro - Work"
        // New format: "Event Name" (just display_name)
        val rawDescription = agg.descriptions.first()
        val projectSuffix = " - ${agg.chronoProject}"
        val meetingName = rawDescription
            .removeSuffix(projectSuffix)
            .replace(dateSuffixRegex, "")
        val sanitized = meetingName.replace(sanitizeRegex, "-")
        val candidates = listOf(
            "$sanitized ${agg.date}.md",       // standard: "Event Name 2026-04-08.md"
            "$sanitized  ${agg.date}.md",      // double-space: trailing whitespace in display_name
            "$sanitized.md"                     // event_name fallback: date already in description
        )

        for (dir in calendarDirs) {
            for (filename in candidates) {
                if (Files.exists(dir.resolve(filename))) {
                    return true
                }
            }
        }

        return false
    }

    private fun roundToQuarter(hours: Double): Double {
        // Round to nearest 0.25
        return (hours / HOUR_INCREMENT).roundToInt() * HOUR_INCREMENT
    }
}
