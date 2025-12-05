package pro.dev.tt

import com.github.ajalt.clikt.core.CliktCommand
import com.github.ajalt.clikt.core.subcommands
import com.github.ajalt.clikt.parameters.options.option
import com.github.ajalt.clikt.parameters.options.required
import kotlinx.coroutines.runBlocking
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.commands.*
import pro.dev.tt.service.BrowserAuthService
import java.io.File

class TtCli : CliktCommand(
    name = "tt",
    help = "Time Tracking Portal CLI"
) {
    override fun run() = Unit
}

fun getToken(): String {
    val tokenFile = File(System.getProperty("user.home"), ".tt-token")

    // Try to get token from file
    if (tokenFile.exists()) {
        val token = tokenFile.readText().trim()
        if (token.isNotEmpty()) {
            return token
        }
    }

    // Try to get token from env var
    val envToken = System.getenv("TT_TOKEN")
    if (envToken != null && envToken.isNotEmpty()) {
        return envToken
    }

    // No token found - trigger browser login
    println("No valid token found. Starting browser authentication...")
    val newToken = BrowserAuthService.refreshTokenViaBrowser()

    if (newToken != null) {
        BrowserAuthService.saveToken(newToken)
        return newToken
    }

    error("Authentication failed. Unable to obtain token.")
}

fun main(args: Array<String>) {
    TtCli()
        .subcommands(
            AuthCommand(),
            FillCommand(),
            ListCommand(),
            ProjectsCommand(),
            CreateCommand(),
            UpdateCommand(),
            DeleteCommand()
        )
        .main(args)
}
