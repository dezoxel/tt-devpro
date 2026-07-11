package pro.dev.tt.commands

/**
 * Pure rendering helpers for settle output, extracted so they can be shared by
 * the interactive draft table and the non-interactive `--dry-run` summary, and
 * unit-tested without a live CLI. Nothing here does I/O — callers decide how to
 * emit the returned strings.
 */

/** Entry type label (Meeting vs Work), independent of filler/borrowed origin. */
internal fun entryType(action: SettleAction): String =
    if (action.isMeeting) "Meeting" else "Work"

/**
 * Chrono entry text: a marker for synthetic entries, otherwise the cleaned
 * description(s) with the trailing " - <chronoProject>" suffix stripped.
 */
internal fun cleanChronoEntry(action: SettleAction): String = when {
    action.isFiller -> "[filler]"
    action.isBorrowed -> "[borrowed]"
    else -> {
        val projectSuffix = " - ${action.aggregate.chronoProject}"
        action.aggregate.descriptions.joinToString("; ") { desc ->
            if (desc.endsWith(projectSuffix)) desc.dropLast(projectSuffix.length) else desc
        }
    }
}

/**
 * Compact, human-readable per-day summary of proposed settle actions.
 * Grouped by date, one indented row per entry, with per-day and grand totals.
 * Pure — returns the block; the caller emits it.
 */
internal fun renderDaySummary(actions: List<SettleAction>): String {
    if (actions.isEmpty()) return "No actions to settle."

    val byDate = actions.groupBy { it.aggregate.date }.toSortedMap()
    val projW = actions.maxOf { it.aggregate.devproProjectName.length }.coerceAtLeast(12)

    val sb = StringBuilder()
    byDate.forEach { (date, dayActions) ->
        val weekday = date.dayOfWeek.name.lowercase().replaceFirstChar { it.uppercase() }.take(3)
        val dayTotal = dayActions.sumOf { it.normalizedHours }
        val entryWord = if (dayActions.size == 1) "entry" else "entries"
        sb.append("%s %s — %d %s → %.2fh\n".format(date, weekday, dayActions.size, entryWord, dayTotal))

        dayActions
            .sortedBy { it.aggregate.devproProjectName }
            .forEach { a ->
                val actionLabel = a.action.name.lowercase().replaceFirstChar { it.uppercase() }
                sb.append("  %-${projW}s  %-7s  %5.2f  %-6s  \"%s\"\n".format(
                    a.aggregate.devproProjectName, entryType(a), a.normalizedHours, actionLabel, a.taskTitle))
            }
        sb.append("\n")
    }

    val grandTotal = actions.sumOf { it.normalizedHours }
    val dayWord = if (byDate.size == 1) "day" else "days"
    sb.append("Total: %.2fh across %d %s, %d entries".format(grandTotal, byDate.size, dayWord, actions.size))
    return sb.toString()
}
