package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.parameters.options.convert
import com.github.ajalt.clikt.parameters.options.option
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.ApiException
import pro.dev.tt.api.ChronoClient
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.config.ConfigLoader
import pro.dev.tt.getToken
import pro.dev.tt.model.CreateWorklogRequest
import pro.dev.tt.model.UpdateWorklogRequest
import pro.dev.tt.model.WorklogDetail
import pro.dev.tt.service.Aggregator
import pro.dev.tt.service.BorrowerService
import pro.dev.tt.service.DayProjectAggregate
import pro.dev.tt.service.FillerService
import pro.dev.tt.service.TimeNormalizer
import java.time.DayOfWeek
import java.time.LocalDate
import java.time.Month
import java.time.format.DateTimeFormatter
import java.time.temporal.TemporalAdjusters

data class SettleAction(
    val aggregate: DayProjectAggregate,
    val normalizedHours: Double,
    val isMeeting: Boolean,
    val isFiller: Boolean,
    val isBorrowed: Boolean = false,
    val sourceDate: LocalDate? = null,  // source date if borrowed
    val taskTitle: String,
    val devproProjectId: String,
    val action: ActionType,
    val existingWorklogId: String? = null,
    val isManuallyFixed: Boolean = false
)

enum class ActionType { CREATE, UPDATE, SKIP }

class SettleCommand : CliktCommand(
    name = "settle",
    help = "Settle daily hours: normalize to 8h, auto-fill gaps, push to DevPro"
) {
    private val from by option("--from", help = "Start date (YYYY-MM-DD). Without --from/--to runs in day-by-day mode")
        .convert { LocalDate.parse(it) }

    private val to by option("--to", help = "End date (YYYY-MM-DD). Without --from/--to runs in day-by-day mode")
        .convert { LocalDate.parse(it) }

    override fun run() { runBlocking {
        val config = try {
            ConfigLoader.load()
        } catch (e: Exception) {
            echo("✗ ${e.message}", err = true)
            return@runBlocking
        }

        val chronoClient = ChronoClient(config.chronoApi)
        val ttClient = TtApiClient(getToken())

        try {
            if (from != null || to != null) {
                // Batch mode: process date range at once
                val rangeFrom = from ?: LocalDate.now().withDayOfMonth(1)
                val rangeTo = to ?: LocalDate.now()
                runBatchMode(rangeFrom, rangeTo, config, chronoClient, ttClient)
            } else {
                // Day-by-day mode: interactive processing one day at a time
                runDayByDayMode(config, chronoClient, ttClient)
            }
        } catch (e: ApiException) {
            echo("✗ API Error: ${e.message}", err = true)
        } catch (e: Exception) {
            echo("✗ Error: ${e.message}", err = true)
        } finally {
            chronoClient.close()
            ttClient.close()
        }
    } }

    private suspend fun runBatchMode(
        from: LocalDate,
        to: LocalDate,
        config: pro.dev.tt.config.Config,
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ) {
        val actions = prepareActions(from, to, config, chronoClient, ttClient)
        if (actions.isEmpty()) return

        echo()
        showDraftTable(actions)

        echo("\n[A]pprove / [C]ancel: ")
        val input = readLine()?.trim()?.lowercase()

        when (input) {
            "a" -> applyAll(actions, ttClient)
            "c", null -> echo("Cancelled.")
            else -> echo("Unknown option. Cancelled.")
        }
    }

    private suspend fun runDayByDayMode(
        config: pro.dev.tt.config.Config,
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ) {
        val today = LocalDate.now()
        val rangeStart = today.minusDays(45)

        echo("Checking last 45 days for unfilled days (<8h)...")

        // Fetch all data for the range (need to query each month separately)
        val user = ttClient.getCurrentUser()

        // Get all months in range
        val months = mutableSetOf<LocalDate>()
        var current = rangeStart.withDayOfMonth(1)
        while (!current.isAfter(today)) {
            months.add(current)
            current = current.plusMonths(1)
        }

        // Fetch DevPro data for each month and merge
        val devproHoursByDay = mutableMapOf<LocalDate, Double>()
        months.forEach { month ->
            val normalView = ttClient.getNormalView(month.format(DateTimeFormatter.ofPattern("yyyy-MM-dd")))
            normalView.pageList
                .flatMap { it.detailsByDates }
                .forEach {
                    val date = LocalDate.parse(it.date.substring(0, 10))
                    devproHoursByDay[date] = it.loggedHours
                }
        }

        // Fetch Chrono entries for the range
        val allEntries = chronoClient.getTimeEntries(rangeStart, today)
        if (allEntries.isEmpty()) {
            echo("No entries found in Chrono for the last 45 days.")
            return
        }

        // Get days that have Chrono data
        val chronoDays = allEntries
            .mapNotNull { entry ->
                entry.startTime.substring(0, 10).let { LocalDate.parse(it) }
            }
            .distinct()
            .sorted()

        // Find unfilled days (have Chrono data AND <8h in DevPro AND not weekend/holiday)
        val unfilledDays = chronoDays.filter { day ->
            val devproHours = devproHoursByDay[day] ?: 0.0
            val isWeekend = day.dayOfWeek == DayOfWeek.SATURDAY || day.dayOfWeek == DayOfWeek.SUNDAY
            devproHours < 8.0 && !isWeekend && !isUSHoliday(day)
        }

        if (unfilledDays.isEmpty()) {
            echo("All days are settled (≥8h logged).")
            return
        }

        echo("${unfilledDays.size} days to settle:")
        unfilledDays.forEach { day ->
            val hours = devproHoursByDay[day] ?: 0.0
            val weekday = day.dayOfWeek.name.lowercase().replaceFirstChar { it.uppercase() }.take(3)
            val hoursInfo = if (hours < 0.01) "" else " (${String.format("%.1f", hours)}h)"
            echo("  $day $weekday$hoursInfo")
        }
        echo()

        // Process day by day
        for (day in unfilledDays) {
            val devproHours = devproHoursByDay[day] ?: 0.0
            val weekday = day.dayOfWeek.name.lowercase().replaceFirstChar { it.uppercase() }
            val currentHours = if (devproHours < 0.01) "empty" else "${String.format("%.1f", devproHours)}h logged"
            echo("═══ $day $weekday ($currentHours) ═══")

            val actions = prepareActions(day, day, config, chronoClient, ttClient)
            if (actions.isEmpty()) {
                echo("No entries for this day.\n")
                continue
            }

            var currentActions = actions

            while (true) {
                showDraftTable(currentActions)

                echo("\n[A]pprove / [E]dit / [S]kip / [C]ancel all: ")
                val input = readLine()?.trim()?.lowercase()

                when (input) {
                    "a" -> {
                        applyAll(currentActions, ttClient)
                        echo()
                        break
                    }
                    "e" -> {
                        currentActions = editEntry(currentActions)
                    }
                    "s" -> {
                        echo("Skipped.\n")
                        break
                    }
                    "c", null -> {
                        echo("Cancelled.")
                        return
                    }
                    else -> {
                        echo("Unknown option. Cancelled.")
                        return
                    }
                }
            }
        }

        echo("Done! All unfilled days processed.")
    }

    private suspend fun prepareActions(
        from: LocalDate,
        to: LocalDate,
        config: pro.dev.tt.config.Config,
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ): List<SettleAction> {
        // 1. Fetch Chrono entries
        echo("Fetching Chrono data ($from to $to)...")
        val entries = chronoClient.getTimeEntries(from, to)

        if (entries.isEmpty()) {
            echo("No entries found in Chrono for this period.")
            return emptyList()
        }

        // 2. Aggregate by date+project (with date filtering)
        val rawAggregates = Aggregator.aggregate(entries, config, from, to)

        if (rawAggregates.isEmpty()) {
            echo("No work entries to process (entries without project or duration are skipped).")
            return emptyList()
        }

        // 3. Normalize to 8h per day
        val normalized = TimeNormalizer.normalize(rawAggregates)

        // 3.5. Generate fillers for meeting-only days
        val fillerEntries = FillerService.generateFillers(normalized, config.fillers)

        // 3.6. Borrow tasks from 7 days ago for remaining sparse days
        val borrowedEntries = BorrowerService.borrowForMeetingOnlyDays(normalized, fillerEntries, chronoClient, config)

        // 4. Get DevPro user and projects
        val user = ttClient.getCurrentUser()
        val projectsResponse = ttClient.getAssignedProjects(user.uniqueId, from.toString())
        val devproProjects = projectsResponse.projects

        // 5. Resolve project IDs (include filler and borrowed projects)
        val aggregates = normalized.map { it.original }
        val fillerProjectNames = fillerEntries.map { it.devproProjectName }.distinct()
        val borrowedProjectNames = borrowedEntries.map { it.devproProjectName }.distinct()
        val allProjectNames = (aggregates.map { it.devproProjectName } + fillerProjectNames + borrowedProjectNames).distinct()
        val projectByName = devproProjects.associateBy { it.shortName.lowercase() }
        val projectIdMap = allProjectNames.associateWith { name ->
            projectByName[name.lowercase()]?.uniqueId
                ?: error("DevPro project '$name' not found")
        }

        // 6. Get existing worklogs from DevPro
        val period = from.format(DateTimeFormatter.ofPattern("yyyy-MM-dd"))
        val normalView = ttClient.getNormalView(period)
        val existingWorklogs = normalView.pageList
            .flatMap { it.detailsByDates }
            .flatMap { day ->
                day.worklogsDetails.map { it to LocalDate.parse(day.date.substring(0, 10)) }
            }

        // 7. Generate task titles from Chrono descriptions
        val datePattern = Regex(", (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \\d{1,2} \\d{4}$")

        val normalActions = normalized.map { norm ->
            val agg = norm.original
            val projectSuffix = " - ${agg.chronoProject}"

            // Use first description directly, strip project suffix and date
            val taskTitle = if (agg.descriptions.isNotEmpty()) {
                agg.descriptions.first()
                    .removeSuffix(projectSuffix)
                    .replace(datePattern, "")
            } else {
                "Development work"
            }
            val devproProjectId = projectIdMap[agg.devproProjectName]!!

            val existing = findExisting(agg.date, devproProjectId, existingWorklogs)

            SettleAction(
                aggregate = agg,
                normalizedHours = norm.normalizedHours,
                isMeeting = norm.isMeeting,
                isFiller = false,
                taskTitle = taskTitle,
                devproProjectId = devproProjectId,
                action = if (existing != null) ActionType.UPDATE else ActionType.CREATE,
                existingWorklogId = existing?.uniqueId
            )
        }

        // 8. Create SettleActions for fillers
        val fillerActions = fillerEntries.map { filler ->
            val devproProjectId = projectIdMap[filler.devproProjectName]!!
            val existing = findExisting(filler.date, devproProjectId, existingWorklogs)

            // Create a synthetic aggregate for the filler
            val syntheticAggregate = DayProjectAggregate(
                date = filler.date,
                chronoProject = "[filler]",
                totalHours = filler.hours,
                descriptions = listOf(filler.taskTitle),
                devproProjectName = filler.devproProjectName,
                billability = filler.billability
            )

            SettleAction(
                aggregate = syntheticAggregate,
                normalizedHours = filler.hours,
                isMeeting = false,
                isFiller = true,
                taskTitle = filler.taskTitle,
                devproProjectId = devproProjectId,
                action = if (existing != null) ActionType.UPDATE else ActionType.CREATE,
                existingWorklogId = existing?.uniqueId
            )
        }

        // 9. Create SettleActions for borrowed entries
        val borrowedActions = borrowedEntries.map { borrowed ->
            val devproProjectId = projectIdMap[borrowed.devproProjectName]!!
            val existing = findExisting(borrowed.date, devproProjectId, existingWorklogs)

            // Create a synthetic aggregate for the borrowed task
            val syntheticAggregate = DayProjectAggregate(
                date = borrowed.date,
                chronoProject = "[borrowed]",
                totalHours = borrowed.hours,
                descriptions = listOf(borrowed.taskTitle),
                devproProjectName = borrowed.devproProjectName,
                billability = borrowed.billability
            )

            SettleAction(
                aggregate = syntheticAggregate,
                normalizedHours = borrowed.hours,
                isMeeting = false,
                isFiller = false,
                isBorrowed = true,
                sourceDate = borrowed.sourceDate,
                taskTitle = borrowed.taskTitle,
                devproProjectId = devproProjectId,
                action = if (existing != null) ActionType.UPDATE else ActionType.CREATE,
                existingWorklogId = existing?.uniqueId
            )
        }

        return (normalActions + fillerActions + borrowedActions)
            .sortedWith(compareBy({ it.aggregate.date }, { it.aggregate.devproProjectName }))
    }

    private fun findExisting(
        date: LocalDate,
        projectId: String,
        existingWorklogs: List<Pair<WorklogDetail, LocalDate>>
    ): WorklogDetail? {
        return existingWorklogs.find { (worklog, worklogDate) ->
            worklogDate == date && worklog.projectUniqueId == projectId
        }?.first
    }

    private fun showDraftTable(actions: List<SettleAction>) {
        data class Row(
            val date: String,
            val chronoProject: String,
            val chronoEntry: String,
            val devproProject: String,
            val taskTitle: String,
            val type: String,
            val originalHours: Double,
            val normalizedHours: Double,
            val action: String
        )

        val rows = actions.map { action ->
            // Determine entry type (Meeting or Work, regardless of filler/borrowed)
            val type = if (action.isMeeting) "Meeting" else "Work"

            // Chrono entry: for filler/borrowed show marker, otherwise clean description
            val chronoEntry = when {
                action.isFiller -> "[filler]"
                action.isBorrowed -> "[borrowed]"
                else -> {
                    val projectSuffix = " - ${action.aggregate.chronoProject}"
                    action.aggregate.descriptions.map { desc ->
                        if (desc.endsWith(projectSuffix)) desc.dropLast(projectSuffix.length) else desc
                    }.joinToString("; ")
                }
            }

            Row(
                date = action.aggregate.date.toString(),
                chronoProject = action.aggregate.chronoProject,
                chronoEntry = chronoEntry,
                devproProject = action.aggregate.devproProjectName,
                taskTitle = action.taskTitle,
                type = type,
                originalHours = action.aggregate.totalHours,
                normalizedHours = action.normalizedHours,
                action = action.action.name.lowercase().replaceFirstChar { it.uppercase() }
            )
        }

        // Calculate dynamic column widths (with min/max constraints)
        val dateW = 10
        val chronoProjW = maxOf(14, rows.maxOfOrNull { it.chronoProject.length } ?: 14)
        val chronoEntryW = minOf(50, maxOf(12, rows.maxOfOrNull { it.chronoEntry.length } ?: 12))
        val devproProjW = maxOf(14, rows.maxOfOrNull { it.devproProject.length } ?: 14)
        val taskTitleW = maxOf(11, rows.maxOfOrNull { it.taskTitle.length } ?: 11)
        val typeW = 8
        val hoursW = 13  // "5.50 → 6.00" format

        // Header
        val header = String.format("%-${dateW}s | %-${chronoProjW}s | %-${chronoEntryW}s | %-${devproProjW}s | %-${taskTitleW}s | %-${typeW}s | %-${hoursW}s | %s",
            "Date", "Chrono Project", "Chrono Entry", "DevPro Project", "DevPro Task", "Type", "Hours", "Action")
        val separator = "-".repeat(header.length)

        echo(header)
        echo(separator)

        rows.forEach { row ->
            val entry = if (row.chronoEntry.length > chronoEntryW)
                row.chronoEntry.take(chronoEntryW - 1) + "…"
                else row.chronoEntry
            val task = if (row.taskTitle.length > taskTitleW)
                row.taskTitle.take(taskTitleW - 1) + "…"
                else row.taskTitle

            // Show hours as "original → normalized" if different, or just normalized if same
            val hoursStr = if (kotlin.math.abs(row.originalHours - row.normalizedHours) < 0.01) {
                String.format("%5.2f", row.normalizedHours)
            } else {
                String.format("%5.2f→%5.2f", row.originalHours, row.normalizedHours)
            }

            val line = String.format("%-${dateW}s | %-${chronoProjW}s | %-${chronoEntryW}s | %-${devproProjW}s | %-${taskTitleW}s | %-${typeW}s | %-${hoursW}s | %s",
                row.date,
                row.chronoProject,
                entry,
                row.devproProject,
                task,
                row.type,
                hoursStr,
                row.action
            )
            echo(line)
        }

        val originalTotal = actions.sumOf { it.aggregate.totalHours }
        val normalizedTotal = actions.sumOf { it.normalizedHours }
        echo(separator)
        echo("Total: %.2f → %.2f hours, %d entries".format(originalTotal, normalizedTotal, actions.size))
    }

    private fun editEntry(actions: List<SettleAction>): List<SettleAction> {
        val editableEntries = actions.filter { !it.isMeeting }
        if (editableEntries.isEmpty()) {
            echo("No editable entries (meetings cannot be edited).")
            return actions
        }

        // Check if there are at least 2 scalable entries (need one to remain scalable after edit)
        val scalableEntries = actions.filter { !it.isMeeting && !it.isManuallyFixed }
        if (scalableEntries.size < 2) {
            echo("✗ Cannot edit: need at least 2 work entries to redistribute hours.")
            return actions
        }

        echo("\nEditable entries:")
        editableEntries.forEachIndexed { index, action ->
            val marker = if (action.isManuallyFixed) "*" else " "
            echo("  ${index + 1}.$marker ${action.aggregate.devproProjectName}: ${action.taskTitle} (${String.format("%.2f", action.normalizedHours)}h)")
        }
        echo("  (* = manually fixed, won't scale)")

        echo("\nEntry number (or 'b' to go back): ")
        val indexInput = readLine()?.trim()
        if (indexInput == "b" || indexInput.isNullOrEmpty()) return actions

        val index = indexInput.toIntOrNull()?.minus(1)
        if (index == null || index < 0 || index >= editableEntries.size) {
            echo("Invalid entry number.")
            return actions
        }

        val selectedAction = editableEntries[index]
        echo("Current: ${String.format("%.2f", selectedAction.normalizedHours)}h. New hours: ")
        val hoursInput = readLine()?.trim()
        if (hoursInput.isNullOrEmpty()) return actions

        val newHours = hoursInput.toDoubleOrNull()
        if (newHours == null || newHours < 0.25) {
            echo("Invalid. Must be >= 0.25")
            return actions
        }

        val roundedHours = (newHours / 0.25).toInt() * 0.25
        val updatedAction = selectedAction.copy(normalizedHours = roundedHours, isManuallyFixed = true)

        val updatedActions = actions.map { if (it === selectedAction) updatedAction else it }
        val result = renormalizeAfterEdit(updatedActions)

        // Block if total < 8h
        val totalHours = result.sumOf { it.normalizedHours }
        if (totalHours < 7.99) {
            echo("✗ Cannot set ${roundedHours}h — would result in ${String.format("%.2f", totalHours)}h total (< 8h)")
            echo("  Minimum for this entry: ${String.format("%.2f", roundedHours + (8.0 - totalHours))}h")
            return actions  // return unchanged
        }

        return result
    }

    private fun renormalizeAfterEdit(actions: List<SettleAction>): List<SettleAction> {
        val fixedHours = actions.filter { it.isMeeting || it.isManuallyFixed }.sumOf { it.normalizedHours }
        val scalableEntries = actions.filter { !it.isMeeting && !it.isManuallyFixed }

        // No scalable entries left - just return as is (user will see warning)
        if (scalableEntries.isEmpty()) return actions

        val scalableHours = scalableEntries.sumOf { it.normalizedHours }
        val targetHours = 8.0 - fixedHours
        if (targetHours <= 0) {
            return actions.map { if (!it.isMeeting && !it.isManuallyFixed) it.copy(normalizedHours = 0.25) else it }
        }

        val scaleFactor = targetHours / scalableHours
        val scaled = scalableEntries.map {
            it.copy(normalizedHours = maxOf(0.25, (it.normalizedHours * scaleFactor / 0.25).toInt() * 0.25))
        }

        val diff = targetHours - scaled.sumOf { it.normalizedHours }
        val finalScaled = if (kotlin.math.abs(diff) >= 0.125 && scaled.isNotEmpty()) {
            val sorted = scaled.sortedByDescending { it.normalizedHours }
            val adjusted = sorted.first().copy(normalizedHours = maxOf(0.25, ((sorted.first().normalizedHours + diff) / 0.25).toInt() * 0.25))
            listOf(adjusted) + sorted.drop(1)
        } else scaled

        val scaledMap = finalScaled.associateBy { it.aggregate }
        return actions.map { scaledMap[it.aggregate] ?: it }
    }

    private suspend fun applyAll(actions: List<SettleAction>, client: TtApiClient) {
        // Fail fast: validate all hours are positive before any API calls
        val invalidActions = actions.filter { it.normalizedHours <= 0 && it.action != ActionType.SKIP }
        if (invalidActions.isNotEmpty()) {
            invalidActions.forEach { action ->
                echo("✗ Invalid hours: ${action.aggregate.date} ${action.aggregate.devproProjectName} (${action.normalizedHours}h)", err = true)
            }
            throw IllegalStateException("Found ${invalidActions.size} entries with non-positive hours. Aborting.")
        }

        var created = 0
        var updated = 0
        var errors = 0

        actions.forEach { action ->
            try {
                when (action.action) {
                    ActionType.CREATE -> {
                        client.createWorklog(CreateWorklogRequest(
                            worklogDate = action.aggregate.date.toString(),
                            projectUniqueId = action.devproProjectId,
                            taskTitle = action.taskTitle,
                            billability = action.aggregate.billability,
                            duration = action.normalizedHours,
                            expenseType = "None"
                        ))
                        created++
                        echo("✓ Created: ${action.aggregate.date} ${action.aggregate.devproProjectName} (${action.normalizedHours}h)")
                    }
                    ActionType.UPDATE -> {
                        client.updateWorklog(UpdateWorklogRequest(
                            uniqueId = action.existingWorklogId!!,
                            worklogDate = action.aggregate.date.toString(),
                            projectUniqueId = action.devproProjectId,
                            taskTitle = action.taskTitle,
                            billability = action.aggregate.billability,
                            duration = action.normalizedHours,
                            expenseType = "None"
                        ))
                        updated++
                        echo("✓ Updated: ${action.aggregate.date} ${action.aggregate.devproProjectName} (${action.normalizedHours}h)")
                    }
                    ActionType.SKIP -> {}
                }
            } catch (e: Exception) {
                errors++
                echo("✗ Failed: ${action.aggregate.date} ${action.aggregate.devproProjectName} - ${e.message}", err = true)
            }
        }

        echo("\nDone! Created: $created, Updated: $updated, Errors: $errors")
    }

    private fun isUSHoliday(date: LocalDate): Boolean {
        val year = date.year

        // Fixed holidays
        val newYearsDay = LocalDate.of(year, Month.JANUARY, 1)
        val independenceDay = LocalDate.of(year, Month.JULY, 4)
        val christmasDay = LocalDate.of(year, Month.DECEMBER, 25)

        // Floating holidays
        // Martin Luther King Jr. Day: 3rd Monday of January
        val mlkDay = LocalDate.of(year, Month.JANUARY, 1)
            .with(TemporalAdjusters.dayOfWeekInMonth(3, DayOfWeek.MONDAY))

        // Memorial Day: Last Monday of May
        val memorialDay = LocalDate.of(year, Month.MAY, 1)
            .with(TemporalAdjusters.lastInMonth(DayOfWeek.MONDAY))

        // Labor Day: 1st Monday of September
        val laborDay = LocalDate.of(year, Month.SEPTEMBER, 1)
            .with(TemporalAdjusters.dayOfWeekInMonth(1, DayOfWeek.MONDAY))

        // Thanksgiving: 4th Thursday of November
        val thanksgiving = LocalDate.of(year, Month.NOVEMBER, 1)
            .with(TemporalAdjusters.dayOfWeekInMonth(4, DayOfWeek.THURSDAY))

        return date == newYearsDay ||
               date == mlkDay ||
               date == memorialDay ||
               date == independenceDay ||
               date == laborDay ||
               date == thanksgiving ||
               date == christmasDay
    }
}
