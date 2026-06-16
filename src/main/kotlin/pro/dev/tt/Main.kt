package pro.dev.tt

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.core.subcommands
import com.github.ajalt.clikt.parameters.options.option
import com.github.ajalt.clikt.parameters.options.required
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.commands.*
import java.io.File

class TtCli : CliktCommand(
    name = "tt",
    help = "Settle Dev.Pro time reports from Chrono"
) {
    override fun run() = Unit
}

// Returns the portal session cookie (`name=value`) used to authenticate API
// calls. Refreshing it requires a host-side browser login (`make auth`), which
// cannot run inside the Docker container — so here we only read it.
fun getSessionCookie(): String {
    val cookieFile = File(System.getProperty("user.home"), ".tt-cookie")

    // Try to get cookie from file
    if (cookieFile.exists()) {
        val cookie = cookieFile.readText().trim()
        if (cookie.isNotEmpty()) {
            return cookie
        }
    }

    // Try to get cookie from env var
    val envCookie = System.getenv("TT_COOKIE")
    if (envCookie != null && envCookie.isNotEmpty()) {
        return envCookie
    }

    error("No Dev.Pro session cookie found. Run 'make auth' on your host machine to create one.")
}

fun main(args: Array<String>) {
    TtCli()
        .subcommands(
            SettleCommand(),
            apiSubcommands()
        )
        .main(args)
}
