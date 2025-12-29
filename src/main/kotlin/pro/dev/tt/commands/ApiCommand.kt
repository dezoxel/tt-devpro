package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.core.subcommands
import com.github.ajalt.clikt.parameters.arguments.argument
import com.github.ajalt.clikt.parameters.options.option
import com.github.ajalt.clikt.parameters.options.required
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.getToken
import pro.dev.tt.model.CreateWorklogRequest
import pro.dev.tt.model.UpdateWorklogRequest

/**
 * Direct API proxy commands for Time Tracking Portal.
 * Useful for debugging, testing, and one-off operations.
 */
class ApiCommand : CliktCommand(
    name = "api",
    help = "Direct API calls to Time Tracking Portal"
) {
    override fun run() = Unit
}

class ApiGetProjectsCommand : CliktCommand(
    name = "get-projects",
    help = "Get assigned projects"
) {
    override fun run() = runBlocking {
        val client = TtApiClient(getToken())

        try {
            val user = client.getCurrentUser()
            val response = client.getAssignedProjects(user.uniqueId, "2025-01-01")

            echo("Assigned Projects:")
            response.projects.forEach { project ->
                echo("  ${project.shortName}: ${project.uniqueId}")
            }
        } catch (e: Exception) {
            echo("✗ Error: ${e.message}", err = true)
        }
    }
}

class ApiGetWorklogsCommand : CliktCommand(
    name = "get-worklogs",
    help = "Get worklogs for a period (normalView endpoint)"
) {
    private val date by option("--date", "-d", help = "Period date (YYYY-MM-DD)").required()

    override fun run() = runBlocking {
        val client = TtApiClient(getToken())

        try {
            val response = client.getNormalView(date)

            response.pageList.forEach { page ->
                page.detailsByDates.forEach { day ->
                    echo("\n=== ${day.date} ===")
                    day.worklogsDetails.forEach { worklog ->
                        echo("  Project: ${worklog.projectShortName}")
                        echo("  Task: ${worklog.taskTitle}")
                        echo("  Hours: ${worklog.loggedHours}")
                        echo("  Billability: ${worklog.billability}")
                        echo("  ExpenseType: ${worklog.expenseType}")
                        echo("  UniqueId: ${worklog.uniqueId}")
                        echo("  ProjectId: ${worklog.projectUniqueId}")
                        echo("  ---")
                    }
                }
            }
        } catch (e: Exception) {
            echo("✗ Error: ${e.message}", err = true)
        }
    }
}

class ApiCreateWorklogCommand : CliktCommand(
    name = "create-worklog",
    help = "Create a new worklog entry"
) {
    private val date by option("--date", "-d", help = "Worklog date (YYYY-MM-DD)").required()
    private val projectId by option("--project-id", "-p", help = "Project uniqueId").required()
    private val task by option("--task", "-t", help = "Task title").required()
    private val billability by option("--billability", "-b", help = "Billable or NonBillable").required()
    private val hours by option("--hours", "-h", help = "Duration in hours").required()
    private val expenseType by option("--expense-type", "-e", help = "Expense type (None, CapEx, OpEx)")
    private val description by option("--description", help = "Optional description")

    override fun run() = runBlocking {
        val client = TtApiClient(getToken())

        val request = CreateWorklogRequest(
            worklogDate = date,
            projectUniqueId = projectId,
            taskTitle = task,
            billability = billability,
            duration = hours.toDouble(),
            expenseType = expenseType,
            description = description
        )

        echo("Creating worklog:")
        echo("  Date: $date")
        echo("  Project: $projectId")
        echo("  Task: $task")
        echo("  Hours: $hours")
        echo("  Billability: $billability")
        echo("  ExpenseType: $expenseType")

        try {
            val success = client.createWorklog(request)
            if (success) {
                echo("✓ Created successfully!")
            } else {
                echo("✗ Create failed", err = true)
            }
        } catch (e: Exception) {
            echo("✗ Error: ${e.message}", err = true)
        }
    }
}

class ApiUpdateWorklogCommand : CliktCommand(
    name = "update-worklog",
    help = "Update an existing worklog entry"
) {
    private val id by option("--id", "-i", help = "Worklog uniqueId").required()
    private val date by option("--date", "-d", help = "Worklog date (YYYY-MM-DD)").required()
    private val projectId by option("--project-id", "-p", help = "Project uniqueId").required()
    private val task by option("--task", "-t", help = "Task title").required()
    private val billability by option("--billability", "-b", help = "Billable or NonBillable").required()
    private val hours by option("--hours", "-h", help = "Duration in hours").required()
    private val expenseType by option("--expense-type", "-e", help = "Expense type (None, CapEx, OpEx)")
    private val description by option("--description", help = "Optional description")

    override fun run() = runBlocking {
        val client = TtApiClient(getToken())

        val request = UpdateWorklogRequest(
            uniqueId = id,
            worklogDate = date,
            projectUniqueId = projectId,
            taskTitle = task,
            billability = billability,
            duration = hours.toDouble(),
            expenseType = expenseType,
            description = description
        )

        echo("Updating worklog: $id")
        echo("  Date: $date")
        echo("  Project: $projectId")
        echo("  Task: $task")
        echo("  Hours: $hours")
        echo("  Billability: $billability")
        echo("  ExpenseType: $expenseType")

        try {
            val success = client.updateWorklog(request)
            if (success) {
                echo("✓ Updated successfully!")
            } else {
                echo("✗ Update failed", err = true)
            }
        } catch (e: Exception) {
            echo("✗ Error: ${e.message}", err = true)
        }
    }
}

class ApiDeleteWorklogCommand : CliktCommand(
    name = "delete-worklog",
    help = "Delete a worklog entry"
) {
    private val id by argument(help = "Worklog uniqueId to delete")

    override fun run() = runBlocking {
        val client = TtApiClient(getToken())

        echo("Deleting worklog: $id")

        try {
            val success = client.deleteWorklog(id)
            if (success) {
                echo("✓ Deleted successfully!")
            } else {
                echo("✗ Delete failed", err = true)
            }
        } catch (e: Exception) {
            echo("✗ Error: ${e.message}", err = true)
        }
    }
}

fun apiSubcommands() = ApiCommand().subcommands(
    ApiGetProjectsCommand(),
    ApiGetWorklogsCommand(),
    ApiCreateWorklogCommand(),
    ApiUpdateWorklogCommand(),
    ApiDeleteWorklogCommand()
)
