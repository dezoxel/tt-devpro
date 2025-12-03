package pro.dev.tt.config

import com.charleskorn.kaml.Yaml
import java.io.File

object ConfigLoader {
    private val configPath = File(System.getProperty("user.home"), ".tt-config.yaml")

    fun load(): Config {
        if (!configPath.exists()) {
            error("""
                Config file not found: ${configPath.absolutePath}

                Create ~/.tt-config.yaml with:

                chrono_api: "http://localhost:9247"
                ollama_api: "http://localhost:11434"
                ollama_model: "llama3.2"

                mappings:
                  - chrono_project: "Your Chrono Project"
                    devpro_project: "DevPro Project Name"
                    billability: "Billable"
            """.trimIndent())
        }

        val content = configPath.readText()
        return try {
            Yaml.default.decodeFromString(Config.serializer(), content)
        } catch (e: Exception) {
            error("Failed to parse config: ${e.message}")
        }
    }
}
