package pro.dev.tt.commands

import com.github.ajalt.clikt.core.CliktCommand
import pro.dev.tt.service.BrowserAuthService

class AuthCommand : CliktCommand(
    name = "auth",
    help = "Refresh authentication token via browser login"
) {
    override fun run() {
        println("Starting browser authentication...")
        println("This will open a browser window for you to log in with your Google account.")
        println()

        val token = BrowserAuthService.refreshTokenViaBrowser()

        if (token != null) {
            BrowserAuthService.saveToken(token)
            println()
            println("✓ Authentication successful!")
            println("Token saved to ~/.tt-token")
        } else {
            println()
            println("✗ Authentication failed")
            throw RuntimeException("Failed to obtain authentication token")
        }
    }
}
