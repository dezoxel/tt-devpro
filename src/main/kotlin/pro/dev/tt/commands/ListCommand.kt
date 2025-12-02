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

class ListCommand : CliktCommand(
    name = "list",
    help = "List worklogs for a period"
) {
    private val period by option("-p", "--period", help = "Period (YYYY-MM-DD), defaults to current month")
        .default(LocalDate.now().withDayOfMonth(1).format(DateTimeFormatter.ISO_DATE))

    override fun run() { runBlocking {
        val client = TtApiClient(getToken())
        try {
            val view = client.getNormalView(period)

            echo("Period: $period")
            echo("Total: ${view.totalLoggedHours}h / ${view.totalExpectedHours}h expected")
            echo()

            view.pageList.firstOrNull()?.detailsByDates?.forEach { day ->
                if (day.worklogsDetails.isNotEmpty()) {
                    val date = day.date.substring(0, 10)
                    echo("[$date] ${day.loggedHours}h")
                    day.worklogsDetails.forEach { w ->
                        echo("  • ${w.projectShortName}: ${w.taskTitle} (${w.loggedHours}h) [${w.uniqueId}]")
                    }
                }
            }
        } catch (e: ApiException) {
            echo("✗ Error: ${e.message}", err = true)
        } finally {
            client.close()
        }
    } }
}
