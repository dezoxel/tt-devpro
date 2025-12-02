package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.parameters.options.default
import com.github.ajalt.clikt.parameters.options.option
import com.github.ajalt.clikt.parameters.options.required
import com.github.ajalt.clikt.parameters.types.double
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.ApiException
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.getToken
import pro.dev.tt.model.CreateWorklogRequest
import java.time.LocalDate
import java.time.format.DateTimeFormatter

class CreateCommand : CliktCommand(
    name = "create",
    help = "Create a new worklog"
) {
    private val projectId by option("-p", "--project", help = "Project UUID").required()
    private val task by option("-t", "--task", help = "Task title").required()
    private val hours by option("-h", "--hours", help = "Duration in hours").double().required()
    private val date by option("-d", "--date", help = "Date (YYYY-MM-DD)")
        .default(LocalDate.now().format(DateTimeFormatter.ISO_DATE))
    private val billable by option("-b", "--billable", help = "Billable (true/false)")
        .default("false")
    private val description by option("--desc", help = "Description")

    override fun run() { runBlocking {
        val client = TtApiClient(getToken())
        try {
            val billability = if (billable.toBoolean()) "Billable" else "NonBillable"
            val worklogDate = "${date}T12:00:00.000Z"

            val request = CreateWorklogRequest(
                worklogDate = worklogDate,
                projectUniqueId = projectId,
                taskTitle = task,
                billability = billability,
                duration = hours,
                description = description
            )

            val success = client.createWorklog(request)
            if (success) {
                echo("✓ Worklog created: $task (${hours}h) on $date")
            } else {
                echo("✗ Failed to create worklog")
            }
        } catch (e: ApiException) {
            echo("✗ Error: ${e.message}", err = true)
        } finally {
            client.close()
        }
    } }
}
