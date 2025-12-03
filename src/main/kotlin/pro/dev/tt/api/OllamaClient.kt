package pro.dev.tt.api

import io.ktor.client.*
import io.ktor.client.call.*
import io.ktor.client.engine.cio.*
import io.ktor.client.plugins.contentnegotiation.*
import io.ktor.client.request.*
import io.ktor.client.statement.*
import io.ktor.http.*
import io.ktor.serialization.kotlinx.json.*
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

@Serializable
data class OllamaRequest(
    val model: String,
    val prompt: String,
    val stream: Boolean = false
)

@Serializable
data class OllamaResponse(
    val response: String
)

class OllamaClient(private val baseUrl: String, private val model: String) {
    private val json = Json {
        ignoreUnknownKeys = true
        isLenient = true
    }

    private val client = HttpClient(CIO) {
        install(ContentNegotiation) {
            json(json)
        }
    }

    suspend fun generateTaskTitle(descriptions: List<String>): String {
        val activitiesText = descriptions
            .filter { it.isNotBlank() }
            .joinToString("\n- ", prefix = "- ")

        if (activitiesText == "- ") {
            return "Development work"
        }

        val prompt = """Summarize these work activities into a concise task title (max 50 chars).
Return ONLY the title, no explanation.

Activities:
$activitiesText"""

        val response = client.post("$baseUrl/api/generate") {
            contentType(ContentType.Application.Json)
            setBody(OllamaRequest(model = model, prompt = prompt))
        }

        when (response.status.value) {
            in 200..299 -> {
                val body = response.bodyAsText()
                // Ollama returns ndjson (multiple JSON lines), collect all response parts
                val fullResponse = body.lines()
                    .filter { it.isNotBlank() }
                    .mapNotNull { line ->
                        try {
                            json.decodeFromString<OllamaResponse>(line).response
                        } catch (e: Exception) {
                            null
                        }
                    }
                    .joinToString("")
                return fullResponse.trim().take(50)
            }
            else -> {
                val body = response.bodyAsText()
                error("Ollama API error (${response.status.value}): $body\n\nIs Ollama running at $baseUrl?")
            }
        }
    }

    fun close() {
        client.close()
    }
}
