package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.parameters.options.convert
import com.github.ajalt.clikt.parameters.options.default
import com.github.ajalt.clikt.parameters.options.flag
import com.github.ajalt.clikt.parameters.options.option
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.ApiException
import pro.dev.tt.api.ChronoClient
import pro.dev.tt.api.OllamaClient
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.config.ConfigLoader
import pro.dev.tt.getToken
import pro.dev.tt.model.CreateWorklogRequest
import pro.dev.tt.model.UpdateWorklogRequest
import pro.dev.tt.model.WorklogDetail
import pro.dev.tt.service.Aggregator
import pro.dev.tt.service.DayProjectAggregate
import java.io.File
import java.time.LocalDate
import java.time.format.DateTimeFormatter

data class FillAction(
    val aggregate: DayProjectAggregate,
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
    private val from by option("--from", help = "Start date (YYYY-MM-DD), defaults to start of current month")
        .convert { LocalDate.parse(it) }
        .default(LocalDate.now().withDayOfMonth(1))

    private val to by option("--to", help = "End date (YYYY-MM-DD), defaults to today")
        .convert { LocalDate.parse(it) }
        .default(LocalDate.now())

    private val dryRun by option("--dry-run", help = "Preview without applying changes")
        .flag()

    override fun run() { runBlocking {
        val config = try {
            ConfigLoader.load()
        } catch (e: Exception) {
            echo("✗ ${e.message}", err = true)
            return@runBlocking
        }

        val chronoClient = ChronoClient(config.chronoApi)
        val ollamaClient = OllamaClient(config.ollamaApi, config.ollamaModel)
        val ttClient = TtApiClient(getToken())

        try {
            // 1. Fetch Chrono entries
            echo("Fetching Chrono data ($from to $to)...")
            val entries = chronoClient.getTimeEntries(from, to)

            if (entries.isEmpty()) {
                echo("No entries found in Chrono for this period.")
                return@runBlocking
            }

            // 2. Aggregate by date+project
            echo("Aggregating entries...")
            val aggregates = Aggregator.aggregate(entries, config)

            if (aggregates.isEmpty()) {
                echo("No work entries to process (entries without project or duration are skipped).")
                return@runBlocking
            }

            // 3. Get DevPro user and projects
            echo("Fetching DevPro data...")
            val user = ttClient.getCurrentUser()
            val projectsResponse = ttClient.getAssignedProjects(user.uniqueId, from.toString())
            val devproProjects = projectsResponse.projects

            // 4. Resolve project IDs
            val projectIdMap = Aggregator.resolveProjectIds(aggregates, devproProjects)

            // 5. Get existing worklogs from DevPro
            val period = from.format(DateTimeFormatter.ofPattern("yyyy-MM-dd"))
            val normalView = ttClient.getNormalView(period)
            val existingWorklogs = normalView.pageList
                .flatMap { it.detailsByDates }
                .flatMap { day ->
                    day.worklogsDetails.map { it to LocalDate.parse(day.date.substring(0, 10)) }
                }

            // 6. Generate task titles (Operations/Meetings = direct, others = Ollama)
            echo("Generating task titles...")
            val datePattern = Regex(", (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \\d{1,2} \\d{4}$")
            val knowledgeBase = "/Users/iurii.buchchenko/knowledge-base"

            val actions = aggregates.map { agg ->
                val isOperations = agg.chronoProject.startsWith("Operations -")
                val projectSuffix = " - ${agg.chronoProject}"

                // Check if this is a meeting by reading Obsidian note
                val isMeeting = if (agg.descriptions.isNotEmpty()) {
                    val noteFile = File("$knowledgeBase/${agg.descriptions.first()}.md")
                    noteFile.exists() && noteFile.readText().contains("The task represents the calendar event")
                } else false

                val taskTitle = if (isOperations && agg.descriptions.isNotEmpty()) {
                    // For Operations: use description directly, strip project suffix and date
                    agg.descriptions.first()
                        .removeSuffix(projectSuffix)
                        .replace(datePattern, "")
                } else if (isMeeting && agg.descriptions.isNotEmpty()) {
                    // For Meetings: use task name (description without project suffix and date)
                    agg.descriptions.first()
                        .removeSuffix(projectSuffix)
                        .replace(datePattern, "")
                } else {
                    ollamaClient.generateTaskTitle(agg.descriptions)
                }
                val devproProjectId = projectIdMap[agg.devproProjectName]!!

                val existing = findExisting(agg.date, devproProjectId, existingWorklogs)

                FillAction(
                    aggregate = agg,
                    taskTitle = taskTitle,
                    devproProjectId = devproProjectId,
                    action = if (existing != null) ActionType.UPDATE else ActionType.CREATE,
                    existingWorklogId = existing?.uniqueId
                )
            }

            // 7. Show draft table
            echo()
            showDraftTable(actions)

            if (dryRun) {
                echo("\n[DRY-RUN] No changes applied.")
                return@runBlocking
            }

            // 8. Interactive review
            echo("\n[A]pply all | [Q]uit")
            val input = readLine()?.trim()?.lowercase()

            when (input) {
                "a" -> applyAll(actions, ttClient)
                "q", null -> echo("Cancelled.")
                else -> echo("Unknown option. Cancelled.")
            }

        } catch (e: ApiException) {
            echo("✗ API Error: ${e.message}", err = true)
        } catch (e: Exception) {
            echo("✗ Error: ${e.message}", err = true)
        } finally {
            chronoClient.close()
            ollamaClient.close()
            ttClient.close()
        }
    } }

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
            val hours: Double,
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
                hours = action.aggregate.totalHours,
                action = action.action.name
            )
        }

        // Calculate dynamic column widths (with min/max constraints)
        val dateW = 10
        val chronoProjW = maxOf(14, rows.maxOfOrNull { it.chronoProject.length } ?: 14)
        val chronoEntriesW = minOf(50, maxOf(14, rows.maxOfOrNull { it.chronoEntries.length } ?: 14))
        val devproProjW = maxOf(12, rows.maxOfOrNull { it.devproProject.length } ?: 12)
        val taskTitleW = maxOf(10, rows.maxOfOrNull { it.taskTitle.length } ?: 10)
        val hoursW = 6
        val actionW = 6

        // Header
        val header = String.format("%-${dateW}s | %-${chronoProjW}s | %-${chronoEntriesW}s | %-${devproProjW}s | %-${taskTitleW}s | %${hoursW}s | %s",
            "Date", "Chrono Project", "Chrono Entries", "DevPro", "Task Title", "Hours", "Action")
        val separator = "-".repeat(header.length)

        echo(header)
        echo(separator)

        rows.forEach { row ->
            val entries = if (row.chronoEntries.length > chronoEntriesW)
                row.chronoEntries.take(chronoEntriesW - 1) + "…"
                else row.chronoEntries
            val task = row.taskTitle

            val line = String.format("%-${dateW}s | %-${chronoProjW}s | %-${chronoEntriesW}s | %-${devproProjW}s | %-${taskTitleW}s | %${hoursW}.2f | %s",
                row.date,
                row.chronoProject,
                entries,
                row.devproProject,
                task,
                row.hours,
                row.action
            )
            echo(line)
        }

        val totalHours = actions.sumOf { it.aggregate.totalHours }
        echo(separator)
        echo("Total: %.2f hours, %d entries".format(totalHours, actions.size))
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
                            duration = action.aggregate.totalHours
                        ))
                        created++
                        echo("✓ Created: ${action.aggregate.date} ${action.aggregate.devproProjectName}")
                    }
                    ActionType.UPDATE -> {
                        client.updateWorklog(UpdateWorklogRequest(
                            uniqueId = action.existingWorklogId!!,
                            worklogDate = action.aggregate.date.toString(),
                            projectUniqueId = action.devproProjectId,
                            taskTitle = action.taskTitle,
                            billability = action.aggregate.billability,
                            duration = action.aggregate.totalHours
                        ))
                        updated++
                        echo("✓ Updated: ${action.aggregate.date} ${action.aggregate.devproProjectName}")
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
