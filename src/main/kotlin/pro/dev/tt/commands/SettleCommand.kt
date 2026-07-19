package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.parameters.options.convert
import com.github.ajalt.clikt.parameters.options.flag
import com.github.ajalt.clikt.parameters.options.option
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.Serializable
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.json.Json as KJson
import pro.dev.tt.api.ApiException
import pro.dev.tt.api.ChronoClient
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.config.ConfigLoader
import pro.dev.tt.getSessionCookie
import pro.dev.tt.model.CreateWorklogRequest
import pro.dev.tt.model.LocalDateSerializer
import pro.dev.tt.model.UpdateWorklogRequest
import pro.dev.tt.model.WorklogDetail
import pro.dev.tt.service.Aggregator
import pro.dev.tt.service.BorrowerService
import pro.dev.tt.service.DayProjectAggregate
import pro.dev.tt.service.FillerBudgetService
import pro.dev.tt.service.FillerService
import pro.dev.tt.service.TimeNormalizer
import java.time.DayOfWeek
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneId
import java.time.format.DateTimeFormatter

@Serializable
data class SettleAction(
    val aggregate: DayProjectAggregate,
    val normalizedHours: Double,
    val isMeeting: Boolean,
    val isFiller: Boolean,
    val isBorrowed: Boolean = false,
    @Serializable(with = LocalDateSerializer::class)
    val sourceDate: LocalDate? = null,  // source date if borrowed
    val taskTitle: String,
    val devproProjectId: String,
    val action: ActionType,
    val existingWorklogId: String? = null,
    val isManuallyFixed: Boolean = false
)

@Serializable
enum class ActionType { CREATE, UPDATE, SKIP }

class SettleCommand : CliktCommand(
    name = "settle",
    help = "Settle daily hours: normalize to 8h, auto-fill gaps, push to DevPro"
) {
    private val from by option("--from", help = "Start date (YYYY-MM-DD). Without --from/--to runs in day-by-day mode")
        .convert { LocalDate.parse(it) }

    private val to by option("--to", help = "End date (YYYY-MM-DD). Without --from/--to runs in day-by-day mode")
        .convert { LocalDate.parse(it) }

    private val json by option("--json", help = "Output proposed actions as JSON and exit without applying")
        .flag(default = false)

    private val dryRun by option("--dry-run", help = "Print a readable per-day summary of proposed actions and exit without applying (--json wins if both are given)")
        .flag(default = false)

    // True whenever output is machine/summary-bound rather than an interactive
    // session: --json, --dry-run, or a non-TTY (piped/automation) run. Status
    // and progress lines go to stderr in these cases so stdout stays clean.
    private val quiet: Boolean get() = json || dryRun || System.console() == null

    override fun run() { runBlocking {
        val config = try {
            ConfigLoader.load()
        } catch (e: Exception) {
            echo("✗ ${e.message}", err = true)
            return@runBlocking
        }

        val chronoClient = ChronoClient(config.chronoApi)
        val ttClient = TtApiClient(getSessionCookie())

        try {
            if (json) {
                // JSON mode: output proposed actions as JSON and exit (wins over --dry-run)
                runJsonMode(config, chronoClient, ttClient)
            } else if (dryRun) {
                // Dry-run: readable per-day summary, no prompts, no apply
                runDryRunMode(config, chronoClient, ttClient)
            } else if (from != null || to != null) {
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

        // No TTY to prompt on (piped/automation): show the readable summary and
        // exit instead of falling through readLine()→null→"Cancelled."
        if (System.console() == null) {
            echo(renderDaySummary(actions))
            return
        }

        echo()
        showDraftTable(actions)
        val hasWarning = showUnderEightWarning(actions)

        val prompt = if (hasWarning) "\n[A]pprove anyway / [C]ancel: " else "\n[A]pprove / [C]ancel: "
        echo(prompt)
        val input = readLine()?.trim()?.lowercase()

        when (input) {
            "a" -> applyAll(actions, ttClient)
            "c", null -> echo("Cancelled.")
            else -> echo("Unknown option. Cancelled.")
        }
    }

    private data class UnfilledDaysResult(
        val unfilledDays: List<LocalDate>,
        val devproHoursByDay: Map<LocalDate, Double>
    )

    private suspend fun findUnfilledDays(
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ): UnfilledDaysResult {
        val today = LocalDate.now()
        val rangeStart = today.minusDays(45)

        echo("Checking last 45 days for unfilled days (<8h)...", err = quiet)

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

        // Fetch Chrono entries for the range (+1 day to catch late-night local entries stored as next UTC day)
        val allEntries = chronoClient.getTimeEntries(rangeStart, today.plusDays(1))
        if (allEntries.isEmpty()) {
            return UnfilledDaysResult(emptyList(), devproHoursByDay)
        }

        // Get days that have Chrono data (convert UTC to local timezone)
        val chronoDays = allEntries
            .mapNotNull { entry ->
                Instant.parse(entry.startTime).atZone(ZoneId.systemDefault()).toLocalDate()
            }
            .distinct()
            .sorted()

        // Find unfilled days (have Chrono data AND <8h in DevPro AND not weekend/holiday)
        val unfilledDays = chronoDays.filter { day ->
            val devproHours = devproHoursByDay[day] ?: 0.0
            val isWeekend = day.dayOfWeek == DayOfWeek.SATURDAY || day.dayOfWeek == DayOfWeek.SUNDAY
            devproHours < 8.0 && !isWeekend && !isUsFederalHoliday(day)
        }

        return UnfilledDaysResult(unfilledDays, devproHoursByDay)
    }

    /**
     * Collect proposed actions for the requested scope, shared by JSON and
     * dry-run modes. With --from/--to → the explicit range; otherwise → the
     * unfilled days in the last 45 days (same semantics as interactive mode).
     */
    private suspend fun collectActions(
        config: pro.dev.tt.config.Config,
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ): List<SettleAction> {
        return if (from != null || to != null) {
            val rangeFrom = from ?: LocalDate.now().withDayOfMonth(1)
            val rangeTo = to ?: LocalDate.now()
            prepareActions(rangeFrom, rangeTo, config, chronoClient, ttClient)
        } else {
            findUnfilledDays(chronoClient, ttClient).unfilledDays.flatMap { day ->
                prepareActions(day, day, config, chronoClient, ttClient)
            }
        }
    }

    private suspend fun runJsonMode(
        config: pro.dev.tt.config.Config,
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ) {
        val allActions = collectActions(config, chronoClient, ttClient)
        val jsonFormat = KJson { prettyPrint = true }
        echo(jsonFormat.encodeToString(ListSerializer(SettleAction.serializer()), allActions))
    }

    private suspend fun runDryRunMode(
        config: pro.dev.tt.config.Config,
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ) {
        val allActions = collectActions(config, chronoClient, ttClient)
        if (allActions.isEmpty()) {
            // For an explicit range, empty can mean "no Chrono/mapped entries"
            // (already reported to stderr) rather than "settled" — stay neutral.
            echo(if (from != null || to != null) "No actions to settle for this range." else "All days are settled (≥8h logged).")
            return
        }
        echo(renderDaySummary(allActions))
    }

    private suspend fun runDayByDayMode(
        config: pro.dev.tt.config.Config,
        chronoClient: ChronoClient,
        ttClient: TtApiClient
    ) {
        val result = findUnfilledDays(chronoClient, ttClient)
        val unfilledDays = result.unfilledDays
        val devproHoursByDay = result.devproHoursByDay

        if (unfilledDays.isEmpty()) {
            echo("All days are settled (≥8h logged).")
            return
        }

        // No TTY to prompt on (piped/automation): can't run the interactive
        // per-day flow, so show the readable summary across all unfilled days
        // and exit instead of hanging/cancelling on readLine().
        if (System.console() == null) {
            val allActions = unfilledDays.flatMap { prepareActions(it, it, config, chronoClient, ttClient) }
            echo(renderDaySummary(allActions))
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
                val hasWarning = showUnderEightWarning(currentActions)

                val prompt = if (hasWarning) "\n[A]pprove anyway / [E]dit / [D]elete / [S]kip / [C]ancel all: " else "\n[A]pprove / [E]dit / [D]elete / [S]kip / [C]ancel all: "
                echo(prompt)
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
                    "d" -> {
                        currentActions = deleteEntry(currentActions)
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
        // 1. Fetch Chrono entries (extend range by +1 day to catch entries that
        // fall on the next UTC day but belong to the local date, e.g. 7:45 PM EST = next day in UTC)
        echo("Fetching Chrono data ($from to $to)...", err = quiet)
        val entries = chronoClient.getTimeEntries(from, to.plusDays(1))

        if (entries.isEmpty()) {
            echo("No entries found in Chrono for this period.", err = quiet)
            return emptyList()
        }

        // 2. Aggregate by date+project (with date filtering)
        val rawAggregates = Aggregator.aggregate(entries, config, from, to)

        if (rawAggregates.isEmpty()) {
            echo("No work entries to process (entries without project or duration are skipped).", err = quiet)
            return emptyList()
        }

        // 3. Normalize to 8h per day
        val normalized = TimeNormalizer.normalize(rawAggregates)

        // 3.4. Fetch existing worklogs for budget calculation
        val period = from.format(DateTimeFormatter.ofPattern("yyyy-MM-dd"))
        val normalView = ttClient.getNormalView(period)
        val existingWorklogs = normalView.pageList
            .flatMap { it.detailsByDates }
            .flatMap { day ->
                day.worklogsDetails.map { it to LocalDate.parse(day.date.substring(0, 10)) }
            }

        // 3.5. Calculate period budgets for fillers
        val billingPeriods = FillerBudgetService.getBillingPeriodsInRange(from, to)
        val periodBudgets = if (config.fillers.any { it.maxHoursPerPeriod != null }) {
            // Calculate remaining budgets for the first billing period (most common case)
            val primaryPeriod = billingPeriods.firstOrNull() ?: FillerBudgetService.getBillingPeriod(from)
            FillerBudgetService.calculateRemainingBudgets(config.fillers, existingWorklogs, primaryPeriod)
        } else {
            null
        }

        // 3.6. Generate fillers for meeting-only days (capped by maxSyntheticHours and period budgets)
        val fillerEntries = FillerService.generateFillers(normalized, config.fillers, config.maxSyntheticHours, periodBudgets)

        // 3.6. Borrow tasks from 7 days ago for remaining sparse days (using remaining synthetic budget)
        val borrowedEntries = BorrowerService.borrowForMeetingOnlyDays(normalized, fillerEntries, chronoClient, config, config.maxSyntheticHours)

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

        // 6. Generate task titles from Chrono descriptions
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

        val allActions = normalActions + fillerActions + borrowedActions
        val adjusted = adjustToEightHours(allActions, config.maxSyntheticHours)
        return adjusted.sortedWith(compareBy({ it.aggregate.date }, { it.aggregate.devproProjectName }))
    }

    /**
     * Final adjustment to ensure each day totals exactly 8h.
     * Adjusts the largest scalable entry to compensate for rounding errors.
     */
    private fun adjustToEightHours(actions: List<SettleAction>, maxSyntheticHours: Double): List<SettleAction> {
        val byDate = actions.groupBy { it.aggregate.date }
        val adjustedByDate = byDate.mapValues { (_, dayActions) ->
            val total = dayActions.sumOf { it.normalizedHours }
            val diff = 8.0 - total

            // Skip if already at 8h (within small tolerance)
            if (kotlin.math.abs(diff) < 0.01) return@mapValues dayActions

            // Calculate current synthetic hours for this day
            val syntheticHours = dayActions.filter { it.isFiller || it.isBorrowed }.sumOf { it.normalizedHours }

            // Find scalable entries (non-meeting, non-manually-fixed)
            val scalable = dayActions.filter { !it.isMeeting && !it.isManuallyFixed }
            if (scalable.isEmpty()) return@mapValues dayActions

            // Prefer non-synthetic entries for adjustment to respect the cap
            val nonSyntheticScalable = scalable.filter { !it.isFiller && !it.isBorrowed }
            val syntheticScalable = scalable.filter { it.isFiller || it.isBorrowed }

            // First try to adjust non-synthetic entries
            if (nonSyntheticScalable.isNotEmpty()) {
                val largest = nonSyntheticScalable.maxByOrNull { it.normalizedHours } ?: return@mapValues dayActions
                val newHours = (((largest.normalizedHours + diff) / 0.25).toInt() * 0.25).coerceAtLeast(0.25)
                return@mapValues dayActions.map { if (it === largest) it.copy(normalizedHours = newHours) else it }
            }

            // Only adjust synthetic if cap allows
            val remainingSyntheticBudget = maxSyntheticHours - syntheticHours
            if (diff > 0 && remainingSyntheticBudget <= 0.01) {
                // Cap reached, don't increase synthetic entries
                return@mapValues dayActions
            }

            // Adjust synthetic entry but only within remaining budget
            val largest = syntheticScalable.maxByOrNull { it.normalizedHours } ?: return@mapValues dayActions
            val adjustAmount = if (diff > 0) kotlin.math.min(diff, remainingSyntheticBudget) else diff
            val newHours = (((largest.normalizedHours + adjustAmount) / 0.25).toInt() * 0.25).coerceAtLeast(0.25)

            dayActions.map { if (it === largest) it.copy(normalizedHours = newHours) else it }
        }
        return adjustedByDate.values.flatten()
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
            Row(
                date = action.aggregate.date.toString(),
                chronoProject = action.aggregate.chronoProject,
                chronoEntry = cleanChronoEntry(action),
                devproProject = action.aggregate.devproProjectName,
                taskTitle = action.taskTitle,
                type = entryType(action),
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

    /**
     * Shows warning if any day has less than 8h due to synthetic hours cap.
     * Returns true if there are days under 8h.
     */
    private fun showUnderEightWarning(actions: List<SettleAction>): Boolean {
        val under = underEightDays(actions)
        if (under.isNotEmpty()) {
            echo("\n⚠️  WARNING: Some days don't reach 8h due to borrowed+filler cap:")
            under.forEach { (date, hours) ->
                echo("  $date: %.2fh (need %.2fh more)".format(hours, 8.0 - hours))
            }
            return true
        }
        return false
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

    private fun deleteEntry(actions: List<SettleAction>): List<SettleAction> {
        val deletableEntries = actions.filter { !it.isMeeting }
        if (deletableEntries.isEmpty()) {
            echo("No deletable entries (meetings cannot be deleted).")
            return actions
        }

        // Must have at least 2 work entries to delete one
        if (deletableEntries.size < 2) {
            echo("✗ Cannot delete: need at least 2 work entries.")
            return actions
        }

        echo("\nDeletable entries:")
        deletableEntries.forEachIndexed { index, action ->
            val marker = if (action.isManuallyFixed) "*" else " "
            echo("  ${index + 1}.$marker ${action.aggregate.devproProjectName}: ${action.taskTitle} (${String.format("%.2f", action.normalizedHours)}h)")
        }

        echo("\nEntry number to delete (or 'b' to go back): ")
        val input = readLine()?.trim()
        if (input == "b" || input.isNullOrEmpty()) return actions

        val index = input.toIntOrNull()?.minus(1)
        if (index == null || index < 0 || index >= deletableEntries.size) {
            echo("Invalid entry number.")
            return actions
        }

        val toDelete = deletableEntries[index]

        // Remove from list
        val remaining = actions.filter { it !== toDelete }

        // Redistribute hours among remaining entries
        val result = renormalizeAfterEdit(remaining)

        echo("✓ Deleted: ${toDelete.taskTitle}")
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
}
