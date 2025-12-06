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
import pro.dev.tt.service.DayProjectAggregate
import pro.dev.tt.service.FillerService
import pro.dev.tt.service.TimeNormalizer
import java.time.DayOfWeek
import java.time.LocalDate
import java.time.Month
import java.time.format.DateTimeFormatter
import java.time.temporal.TemporalAdjusters

data class FillAction(
    val aggregate: DayProjectAggregate,
    val normalizedHours: Double,
    val isMeeting: Boolean,
    val isFiller: Boolean,
    val taskTitle: String,
    val devproProjectId: String,
    val action: ActionType,
    val existingWorklogId: String? = null
)

enum class ActionType { CREATE, UPDATE, SKIP }

class FillCommand : CliktCommand(
    name = "fill",
    help = "Fill DevPro time tracking from Chrono data"
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

        echo("Day-by-day mode: checking last 45 days for unfilled days (<8h)...")

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
            echo("All days with Chrono data are filled (≥8h in DevPro).")
            return
        }

        echo("Found ${unfilledDays.size} unfilled day(s):")
        unfilledDays.forEach { day ->
            val hours = devproHoursByDay[day] ?: 0.0
            echo("  $day (${day.dayOfWeek}): ${String.format("%.1f", hours)}h in DevPro")
        }
        echo()

        // Process day by day
        for (day in unfilledDays) {
            val devproHours = devproHoursByDay[day] ?: 0.0
            echo("═══ ${day} (DevPro: ${String.format("%.1f", devproHours)}h) ═══")

            val actions = prepareActions(day, day, config, chronoClient, ttClient)
            if (actions.isEmpty()) {
                echo("No entries for this day.\n")
                continue
            }

            showDraftTable(actions)

            echo("\n[A]pprove / [S]kip / [C]ancel all: ")
            val input = readLine()?.trim()?.lowercase()

            when (input) {
                "a" -> {
                    applyAll(actions, ttClient)
                    echo()
                }
                "s" -> {
                    echo("Skipped.\n")
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

        echo("Done! All unfilled days processed.")
    }

    private suspend fun prepareActions(
        from: LocalDate,
        to: LocalDate,
        config: pro.dev.tt.config.Config,
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ): List<FillAction> {
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

        // 4. Get DevPro user and projects
        val user = ttClient.getCurrentUser()
        val projectsResponse = ttClient.getAssignedProjects(user.uniqueId, from.toString())
        val devproProjects = projectsResponse.projects

        // 5. Resolve project IDs (include filler projects)
        val aggregates = normalized.map { it.original }
        val fillerProjectNames = fillerEntries.map { it.devproProjectName }.distinct()
        val allProjectNames = (aggregates.map { it.devproProjectName } + fillerProjectNames).distinct()
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

            FillAction(
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

        // 8. Create FillActions for fillers
        val fillerActions = fillerEntries.map { filler ->
            val devproProjectId = projectIdMap[filler.devproProjectName]!!
            val existing = findExisting(filler.date, devproProjectId, existingWorklogs)

            // Create a synthetic aggregate for the filler
            val syntheticAggregate = DayProjectAggregate(
                date = filler.date,
                chronoProject = "[FILLER]",
                totalHours = filler.hours,
                descriptions = listOf(filler.taskTitle),
                devproProjectName = filler.devproProjectName,
                billability = filler.billability
            )

            FillAction(
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

        return (normalActions + fillerActions)
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

    private fun showDraftTable(actions: List<FillAction>) {
        // Prepare data with Chrono entries as comma-separated list
        data class Row(
            val date: String,
            val chronoProject: String,
            val chronoEntries: String,
            val devproProject: String,
            val taskTitle: String,
            val originalHours: Double,
            val normalizedHours: Double,
            val isMeeting: Boolean,
            val isFiller: Boolean,
            val action: String
        )

        val rows = actions.map { action ->
            // Strip project name suffix from descriptions (e.g., " - Operations - Artory - DevPro - Work")
            val projectSuffix = " - ${action.aggregate.chronoProject}"
            val cleanDescriptions = action.aggregate.descriptions.map { desc ->
                if (desc.endsWith(projectSuffix)) desc.dropLast(projectSuffix.length) else desc
            }
            Row(
                date = action.aggregate.date.toString(),
                chronoProject = action.aggregate.chronoProject,
                chronoEntries = cleanDescriptions.joinToString("; "),
                devproProject = action.aggregate.devproProjectName,
                taskTitle = action.taskTitle,
                originalHours = action.aggregate.totalHours,
                normalizedHours = action.normalizedHours,
                isMeeting = action.isMeeting,
                isFiller = action.isFiller,
                action = action.action.name
            )
        }

        // Calculate dynamic column widths (with min/max constraints)
        val dateW = 10
        val chronoProjW = maxOf(14, rows.maxOfOrNull { it.chronoProject.length } ?: 14)
        val chronoEntriesW = minOf(50, maxOf(14, rows.maxOfOrNull { it.chronoEntries.length } ?: 14))
        val devproProjW = maxOf(12, rows.maxOfOrNull { it.devproProject.length } ?: 12)
        val taskTitleW = maxOf(10, rows.maxOfOrNull { it.taskTitle.length } ?: 10)
        val hoursW = 13  // "5.50 → 6.00" format
        val actionW = 6

        // Header
        val header = String.format("%-${dateW}s | %-${chronoProjW}s | %-${chronoEntriesW}s | %-${devproProjW}s | %-${taskTitleW}s | %-${hoursW}s | %s",
            "Date", "Chrono Project", "Chrono Entries", "DevPro", "Task Title", "Hours", "Action")
        val separator = "-".repeat(header.length)

        echo(header)
        echo(separator)

        rows.forEach { row ->
            val entries = if (row.chronoEntries.length > chronoEntriesW)
                row.chronoEntries.take(chronoEntriesW - 1) + "…"
                else row.chronoEntries
            val task = row.taskTitle

            // Show hours as "original → normalized" if different, or just normalized if same
            val hoursStr = if (kotlin.math.abs(row.originalHours - row.normalizedHours) < 0.01) {
                String.format("%5.2f", row.normalizedHours)
            } else {
                val marker = if (row.isMeeting) "*" else ""
                String.format("%5.2f→%5.2f%s", row.originalHours, row.normalizedHours, marker)
            }

            // Add filler marker to action
            val actionStr = if (row.isFiller) "${row.action}+" else row.action

            val line = String.format("%-${dateW}s | %-${chronoProjW}s | %-${chronoEntriesW}s | %-${devproProjW}s | %-${taskTitleW}s | %-${hoursW}s | %s",
                row.date,
                row.chronoProject,
                entries,
                row.devproProject,
                task,
                hoursStr,
                actionStr
            )
            echo(line)
        }

        val originalTotal = actions.sumOf { it.aggregate.totalHours }
        val normalizedTotal = actions.sumOf { it.normalizedHours }
        val fillerCount = actions.count { it.isFiller }
        echo(separator)
        val legend = mutableListOf<String>()
        if (kotlin.math.abs(originalTotal - normalizedTotal) >= 0.01) {
            legend.add("* = meeting, not scaled")
        }
        if (fillerCount > 0) {
            legend.add("+ = auto-filler")
        }
        val legendStr = if (legend.isNotEmpty()) " (${legend.joinToString(", ")})" else ""
        echo("Total: %.2f → %.2f hours, %d entries%s".format(originalTotal, normalizedTotal, actions.size, legendStr))
    }

    private suspend fun applyAll(actions: List<FillAction>, client: TtApiClient) {
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
                            duration = action.normalizedHours
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
                            duration = action.normalizedHours
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
