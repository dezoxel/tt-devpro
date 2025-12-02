package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.parameters.options.option
import com.github.ajalt.clikt.parameters.options.required
import com.github.ajalt.clikt.parameters.types.double
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.ApiException
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.getToken
import pro.dev.tt.model.UpdateWorklogRequest

class UpdateCommand : CliktCommand(
    name = "update",
    help = "Update an existing worklog"
) {
    private val id by option("-i", "--id", help = "Worklog UUID").required()
    private val projectId by option("-p", "--project", help = "Project UUID").required()
    private val task by option("-t", "--task", help = "Task title").required()
    private val hours by option("-h", "--hours", help = "Duration in hours").double().required()
    private val date by option("-d", "--date", help = "Date (YYYY-MM-DD)").required()
    private val billable by option("-b", "--billable", help = "Billable (true/false)")
    private val description by option("--desc", help = "Description")

    override fun run() { runBlocking {
        val client = TtApiClient(getToken())
        try {
            val billability = if (billable?.toBoolean() == true) "Billable" else "NonBillable"
            val worklogDate = "${date}T00:00:00"

            val request = UpdateWorklogRequest(
                uniqueId = id,
                worklogDate = worklogDate,
                projectUniqueId = projectId,
                taskTitle = task,
                billability = billability,
                duration = hours,
                description = description
            )

            val success = client.updateWorklog(request)
            if (success) {
                echo("✓ Worklog updated: $task (${hours}h)")
            } else {
                echo("✗ Failed to update worklog")
            }
        } catch (e: ApiException) {
            echo("✗ Error: ${e.message}", err = true)
        } finally {
            client.close()
        }
    } }
}
