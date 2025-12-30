package pro.dev.tt.service

import pro.dev.tt.config.Filler
import java.time.LocalDate
import kotlin.math.min
import kotlin.math.roundToInt
import kotlin.random.Random

/**
 * Service for auto-filling meeting-only days with filler activities.
 * When a day consists only of meetings and doesn't reach 8h,
 * random fillers are added to fill the remaining time.
 */
object FillerService {
    private const val TARGET_HOURS = 8.0
    private const val HOUR_INCREMENT = 0.25

    data class FillerEntry(
        val date: LocalDate,
        val devproProjectName: String,
        val taskTitle: String,
        val billability: String,
        val hours: Double
    )

    /**
     * Adds filler entries to meeting-only days that don't reach 8h.
     * Returns a list of filler entries to be added.
     * @param maxSyntheticHours maximum total hours for fillers per day (part of borrowed+filler cap)
     * @param periodBudgets mutable map tracking remaining budget per filler for the billing period
     */
    fun generateFillers(
        normalized: List<TimeNormalizer.NormalizedAggregate>,
        fillers: List<Filler>,
        maxSyntheticHours: Double,
        periodBudgets: MutableMap<FillerBudgetService.FillerKey, Double>? = null
    ): List<FillerEntry> {
        if (fillers.isEmpty()) return emptyList()

        // Group by date
        val byDate = normalized.groupBy { it.original.date }

        return byDate.flatMap { (date, dayEntries) ->
            generateFillersForDay(date, dayEntries, fillers, maxSyntheticHours, periodBudgets)
        }
    }

    private fun generateFillersForDay(
        date: LocalDate,
        dayEntries: List<TimeNormalizer.NormalizedAggregate>,
        fillers: List<Filler>,
        maxSyntheticHours: Double,
        periodBudgets: MutableMap<FillerBudgetService.FillerKey, Double>?
    ): List<FillerEntry> {
        // Check if all entries are meetings
        val allMeetings = dayEntries.all { it.isMeeting }
        if (!allMeetings) return emptyList()

        // Calculate remaining hours needed
        val currentHours = dayEntries.sumOf { it.normalizedHours }
        val remainingHours = TARGET_HOURS - currentHours

        if (remainingHours <= 0) return emptyList()

        // Cap fillers to maxSyntheticHours (leave room for borrowed entries)
        val cappedRemainingHours = min(remainingHours, maxSyntheticHours)

        // Get projects that are already present in this day
        val presentProjects = dayEntries.map { it.original.devproProjectName }.toSet()

        // Generate fillers to fill the gap, but only for projects already present
        return fillGap(date, cappedRemainingHours, fillers, presentProjects, periodBudgets)
    }

    private fun fillGap(
        date: LocalDate,
        remainingHours: Double,
        fillers: List<Filler>,
        presentProjects: Set<String>,
        periodBudgets: MutableMap<FillerBudgetService.FillerKey, Double>?
    ): List<FillerEntry> {
        val result = mutableListOf<FillerEntry>()
        var hoursToFill = remainingHours
        val usedProjects = mutableSetOf<String>()  // Track which projects already got a filler today

        // Keep adding fillers until we've filled the gap
        while (hoursToFill > 0 && hoursToFill >= HOUR_INCREMENT) {
            // Select random filler from projects that:
            // 1. Are present in this day
            // 2. Haven't been used yet for fillers
            // 3. Have remaining period budget (if budget tracking is enabled)
            val availableFillers = fillers.filter { filler ->
                val isPresent = presentProjects.contains(filler.devproProject)
                val notUsedToday = !usedProjects.contains(filler.devproProject)
                val hasBudget = periodBudgets?.let {
                    FillerBudgetService.hasRemainingBudget(it, filler.devproProject, filler.taskTitle)
                } ?: true

                isPresent && notUsedToday && hasBudget
            }

            if (availableFillers.isEmpty()) {
                // No more available projects for fillers, break
                break
            }

            val filler = availableFillers.random()
            usedProjects.add(filler.devproProject)

            // Calculate hours for this filler (within its range, but not more than needed)
            var maxAllowed = min(filler.maxHours, hoursToFill)

            // Also cap by remaining period budget
            if (periodBudgets != null) {
                val key = FillerBudgetService.FillerKey(filler.devproProject, filler.taskTitle)
                val remainingBudget = periodBudgets[key] ?: Double.MAX_VALUE
                if (remainingBudget < Double.MAX_VALUE) {
                    maxAllowed = min(maxAllowed, remainingBudget)
                }
            }

            val minAllowed = min(filler.minHours, maxAllowed)

            // Skip if we can't meet minimum
            if (maxAllowed < HOUR_INCREMENT) {
                continue
            }

            // Generate random hours within allowed range
            val rawHours = if (maxAllowed <= minAllowed) {
                minAllowed
            } else {
                Random.nextDouble(minAllowed, maxAllowed)
            }

            // Round to 0.25h increments
            val hours = roundToQuarter(rawHours)

            if (hours > 0) {
                // Consume from period budget
                val consumed = periodBudgets?.let {
                    FillerBudgetService.consumeBudget(it, filler.devproProject, filler.taskTitle, hours)
                } ?: hours

                if (consumed >= HOUR_INCREMENT) {
                    result.add(FillerEntry(
                        date = date,
                        devproProjectName = filler.devproProject,
                        taskTitle = filler.taskTitle,
                        billability = filler.billability,
                        hours = roundToQuarter(consumed)
                    ))
                    hoursToFill -= roundToQuarter(consumed)
                }
            } else {
                // Can't add more fillers, break to avoid infinite loop
                break
            }
        }

        return result
    }

    private fun roundToQuarter(hours: Double): Double {
        return (hours / HOUR_INCREMENT).roundToInt() * HOUR_INCREMENT
    }
}
