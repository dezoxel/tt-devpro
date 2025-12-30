package pro.dev.tt.service

import pro.dev.tt.config.Filler
import pro.dev.tt.model.WorklogDetail
import java.time.LocalDate

/**
 * Service for tracking filler usage budgets across billing periods.
 * Billing periods are: 1-15 and 16-end of each month.
 */
object FillerBudgetService {

    data class FillerKey(
        val devproProject: String,
        val taskTitle: String
    )

    data class BillingPeriod(
        val start: LocalDate,
        val end: LocalDate
    ) {
        fun contains(date: LocalDate): Boolean {
            return !date.isBefore(start) && !date.isAfter(end)
        }
    }

    /**
     * Determines the billing period for a given date.
     * Period 1: 1st to 15th of the month
     * Period 2: 16th to end of the month
     */
    fun getBillingPeriod(date: LocalDate): BillingPeriod {
        return if (date.dayOfMonth <= 15) {
            BillingPeriod(
                start = date.withDayOfMonth(1),
                end = date.withDayOfMonth(15)
            )
        } else {
            BillingPeriod(
                start = date.withDayOfMonth(16),
                end = date.withDayOfMonth(date.lengthOfMonth())
            )
        }
    }

    /**
     * Gets all unique billing periods that span the given date range.
     */
    fun getBillingPeriodsInRange(startDate: LocalDate, endDate: LocalDate): List<BillingPeriod> {
        val periods = mutableSetOf<BillingPeriod>()
        var current = startDate
        while (!current.isAfter(endDate)) {
            periods.add(getBillingPeriod(current))
            current = current.plusDays(1)
        }
        return periods.toList().sortedBy { it.start }
    }

    /**
     * Calculates remaining budget for each filler based on existing worklogs.
     *
     * @param fillers List of filler configurations with optional maxHoursPerPeriod
     * @param existingWorklogs List of existing worklogs with their dates
     * @param billingPeriod The billing period to calculate budgets for
     * @return Map of FillerKey to remaining hours (null maxHoursPerPeriod = Double.MAX_VALUE)
     */
    fun calculateRemainingBudgets(
        fillers: List<Filler>,
        existingWorklogs: List<Pair<WorklogDetail, LocalDate>>,
        billingPeriod: BillingPeriod
    ): MutableMap<FillerKey, Double> {
        val budgets = mutableMapOf<FillerKey, Double>()

        // Initialize budgets from filler configs
        for (filler in fillers) {
            val key = FillerKey(filler.devproProject, filler.taskTitle)
            val maxBudget = filler.maxHoursPerPeriod ?: Double.MAX_VALUE
            budgets[key] = maxBudget
        }

        // Subtract hours from existing worklogs in this billing period
        for ((worklog, date) in existingWorklogs) {
            if (!billingPeriod.contains(date)) continue

            val key = FillerKey(worklog.projectShortName, worklog.taskTitle)
            val currentBudget = budgets[key]
            if (currentBudget != null && currentBudget < Double.MAX_VALUE) {
                budgets[key] = maxOf(0.0, currentBudget - worklog.loggedHours)
            }
        }

        return budgets
    }

    /**
     * Checks if a filler has remaining budget.
     */
    fun hasRemainingBudget(
        budgets: Map<FillerKey, Double>,
        devproProject: String,
        taskTitle: String,
        minRequired: Double = 0.25
    ): Boolean {
        val key = FillerKey(devproProject, taskTitle)
        val remaining = budgets[key] ?: Double.MAX_VALUE
        return remaining >= minRequired
    }

    /**
     * Consumes budget for a filler and returns the consumed amount (may be less than requested).
     */
    fun consumeBudget(
        budgets: MutableMap<FillerKey, Double>,
        devproProject: String,
        taskTitle: String,
        requestedHours: Double
    ): Double {
        val key = FillerKey(devproProject, taskTitle)
        val available = budgets[key] ?: Double.MAX_VALUE

        return if (available >= Double.MAX_VALUE - 1) {
            // No limit configured, allow full amount
            requestedHours
        } else {
            // Consume up to available budget
            val consumed = minOf(requestedHours, available)
            budgets[key] = available - consumed
            consumed
        }
    }
}
