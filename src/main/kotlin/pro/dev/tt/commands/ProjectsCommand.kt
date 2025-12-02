package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.parameters.options.default
import com.github.ajalt.clikt.parameters.options.option
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.ApiException
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.getToken
import java.time.LocalDate
import java.time.format.DateTimeFormatter

class ProjectsCommand : CliktCommand(
    name = "projects",
    help = "List assigned projects"
) {
    private val date by option("-d", "--date", help = "Date (YYYY-MM-DD)")
        .default(LocalDate.now().format(DateTimeFormatter.ISO_DATE))

    override fun run() { runBlocking {
        val client = TtApiClient(getToken())
        try {
            val user = client.getCurrentUser()
            echo("User: ${user.fullName}")
            echo()

            val projects = client.getAssignedProjects(user.uniqueId, date)
            echo("Assigned projects:")
            projects.projects
                .sortedByDescending { it.isFavorite }
                .forEach { p ->
                    val fav = if (p.isFavorite) "★" else " "
                    echo("  $fav ${p.shortName}")
                    echo("    ID: ${p.uniqueId}")
                }
        } catch (e: ApiException) {
            echo("✗ Error: ${e.message}", err = true)
        } finally {
            client.close()
        }
    } }
}
