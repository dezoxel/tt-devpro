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
    help = "Time Tracking Portal CLI"
) {
    override fun run() = Unit
}

fun getToken(): String {
    val tokenFile = File(System.getProperty("user.home"), ".tt-token")
    return if (tokenFile.exists()) {
        tokenFile.readText().trim()
    } else {
        System.getenv("TT_TOKEN")
            ?: error("Token not found. Set TT_TOKEN env var or create ~/.tt-token file")
    }
}

fun main(args: Array<String>) {
    TtCli()
        .subcommands(
            FillCommand(),
            ListCommand(),
            ProjectsCommand(),
            CreateCommand(),
            UpdateCommand(),
            DeleteCommand()
        )
        .main(args)
}
