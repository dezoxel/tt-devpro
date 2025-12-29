package pro.dev.tt.service

import pro.dev.tt.api.ChronoClient
import pro.dev.tt.config.Config
import pro.dev.tt.model.ChronoTimeEntry
import java.time.LocalDate
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * Service for borrowing tasks from 7 days ago to fill meeting-only days.
 * When a day consists only of meetings and doesn't reach 8h even after fillers,
 * this service borrows tasks from 7 days ago using LLM to select contextually appropriate ones.
 */
object BorrowerService {
    private const val TARGET_HOURS = 8.0
    private const val HOUR_INCREMENT = 0.25
    private const val LOOKBACK_DAYS = 7

    data class BorrowedEntry(
        val date: LocalDate,
        val sourceDate: LocalDate,  // the date we borrowed from
        val devproProjectName: String,
        val taskTitle: String,
        val billability: String,
        val hours: Double
    )

    /**
     * Borrows tasks from 7 days ago for meeting-only days that don't reach 8h.
     * Returns a list of borrowed entries to be added.
     */
    suspend fun borrowForMeetingOnlyDays(
        normalized: List<TimeNormalizer.NormalizedAggregate>,
        fillers: List<FillerService.FillerEntry>,
        chronoClient: ChronoClient,
        config: Config
    ): List<BorrowedEntry> {
        // Group by date to find days with shortfall even after fillers
        val byDate = normalized.groupBy { it.original.date }
        val daysWithShortfall = byDate
            .mapNotNull { (date, dayEntries) ->
                val entryHours = dayEntries.sumOf { it.normalizedHours }
                val fillerHours = fillers
                    .filter { it.date == date }
                    .sumOf { it.hours }
                val totalHours = entryHours + fillerHours
                val shortfall = TARGET_HOURS - totalHours

                // Only consider if there's a meaningful shortfall (and day is meeting-heavy)
                // Check if day consists primarily of meetings (meeting entries > work entries in hours)
                val meetingHours = dayEntries.filter { it.isMeeting }.sumOf { it.normalizedHours }
                val workHours = dayEntries.filter { !it.isMeeting }.sumOf { it.normalizedHours }
                val isMeetingHeavy = meetingHours >= workHours

                if (shortfall > HOUR_INCREMENT && isMeetingHeavy) {
                    date to shortfall
                } else {
                    null
                }
            }
            .toMap()

        if (daysWithShortfall.isEmpty()) {
            return emptyList()
        }

        // Fetch historical data from 7 days ago
        val earliestDate = byDate.keys.minOrNull() ?: return emptyList()
        val historyStartDate = earliestDate.minusDays(LOOKBACK_DAYS.toLong())
        val historyEndDate = earliestDate.minusDays(1)

        val historyEntries = try {
            chronoClient.getTimeEntries(historyStartDate, historyEndDate)
        } catch (e: Exception) {
            // If we can't fetch history, just skip borrowing
            return emptyList()
        }

        if (historyEntries.isEmpty()) {
            return emptyList()
        }

        // Aggregate historical entries to find candidate tasks
        val historicalAggregates = Aggregator.aggregate(historyEntries, config, historyStartDate, historyEndDate)

        // For each day with shortfall, borrow tasks
        return daysWithShortfall.flatMap { (date, shortfall) ->
            borrowForDay(date, shortfall, historicalAggregates, config)
        }
    }

    private fun borrowForDay(
        date: LocalDate,
        shortfall: Double,
        historicalAggregates: List<DayProjectAggregate>,
        config: Config
    ): List<BorrowedEntry> {
        if (historicalAggregates.isEmpty()) return emptyList()

        // For now, use a simple strategy: select most frequent tasks from historical data
        // TODO: integrate LLM to make smarter selections based on context

        // Flatten all descriptions and track their hours
        val taskFrequency = mutableMapOf<String, Pair<Double, DayProjectAggregate>>()
        for (agg in historicalAggregates) {
            for (desc in agg.descriptions) {
                val key = desc
                val (hours, _) = taskFrequency[key] ?: (0.0 to agg)
                taskFrequency[key] = (hours + agg.totalHours / agg.descriptions.size.coerceAtLeast(1)) to agg
            }
        }

        val sortedTasks = taskFrequency.entries
            .sortedByDescending { it.value.first }
            .map { it.key to it.value.second }

        if (sortedTasks.isEmpty()) return emptyList()

        val result = mutableListOf<BorrowedEntry>()
        var hoursToFill = shortfall
        var taskIndex = 0

        // Keep borrowing tasks until we've filled the shortfall
        while (hoursToFill >= HOUR_INCREMENT && taskIndex < sortedTasks.size) {
            val (taskDescription, sourceAggregate) = sortedTasks[taskIndex]

            // Calculate hours for this borrowed task
            val maxAllowed = min(sourceAggregate.totalHours / sourceAggregate.descriptions.size.coerceAtLeast(1), hoursToFill)
            val hours = roundToQuarter(maxAllowed)

            if (hours > 0) {
                // Clean task title: remove project suffix and date pattern
                val projectSuffix = " - ${sourceAggregate.chronoProject}"
                val datePattern = Regex(", (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \\d{1,2} \\d{4}$")
                val cleanTitle = taskDescription
                    .removeSuffix(projectSuffix)
                    .replace(datePattern, "")

                result.add(BorrowedEntry(
                    date = date,
                    sourceDate = sourceAggregate.date,
                    devproProjectName = sourceAggregate.devproProjectName,
                    taskTitle = cleanTitle,
                    billability = sourceAggregate.billability,
                    hours = hours
                ))
                hoursToFill -= hours
            }

            taskIndex++
        }

        return result
    }

    private fun roundToQuarter(hours: Double): Double {
        return (hours / HOUR_INCREMENT).roundToInt() * HOUR_INCREMENT
    }
}
