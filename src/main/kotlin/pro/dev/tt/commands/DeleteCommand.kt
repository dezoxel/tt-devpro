package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.parameters.options.option
import com.github.ajalt.clikt.parameters.options.required
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.ApiException
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.getToken

class DeleteCommand : CliktCommand(
    name = "delete",
    help = "Delete a worklog"
) {
    private val id by option("-i", "--id", help = "Worklog UUID").required()

    override fun run() { runBlocking {
        val client = TtApiClient(getToken())
        try {
            val success = client.deleteWorklog(id)
            if (success) {
                echo("✓ Worklog deleted: $id")
            } else {
                echo("✗ Failed to delete worklog")
            }
        } catch (e: ApiException) {
            echo("✗ Error: ${e.message}", err = true)
        } finally {
            client.close()
        }
    } }
}
