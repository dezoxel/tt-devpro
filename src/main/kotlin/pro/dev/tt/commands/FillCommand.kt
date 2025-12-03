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

            // 6. Generate task titles via Ollama
            echo("Generating task titles...")
            val actions = aggregates.map { agg ->
                val taskTitle = ollamaClient.generateTaskTitle(agg.descriptions)
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
        val header = String.format("%-12s | %-15s | %6s | %-30s | %s",
            "Date", "Project", "Hours", "Task Title", "Action")
        val separator = "-".repeat(header.length)

        echo(header)
        echo(separator)

        actions.forEach { action ->
            val line = String.format("%-12s | %-15s | %6.2f | %-30s | %s",
                action.aggregate.date,
                action.aggregate.devproProjectName.take(15),
                action.aggregate.totalHours,
                action.taskTitle.take(30),
                action.action.name
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
