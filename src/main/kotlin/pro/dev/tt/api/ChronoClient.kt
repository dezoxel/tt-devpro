package pro.dev.tt.api

import io.ktor.client.*
import io.ktor.client.call.*
import io.ktor.client.engine.cio.*
import io.ktor.client.plugins.contentnegotiation.*
import io.ktor.client.request.*
import io.ktor.client.statement.*
import io.ktor.serialization.kotlinx.json.*
import kotlinx.serialization.json.Json
import pro.dev.tt.model.ChronoTimeEntry
import java.time.LocalDate

class ChronoClient(private val baseUrl: String) {
    private val json = Json {
        ignoreUnknownKeys = true
        isLenient = true
    }

    private val client = HttpClient(CIO) {
        install(ContentNegotiation) {
            json(json)
        }
    }

    suspend fun getTimeEntries(startDate: LocalDate, endDate: LocalDate): List<ChronoTimeEntry> {
        val response = client.get("$baseUrl/api/time-entries") {
            parameter("start_date", startDate.toString())
            parameter("end_date", endDate.toString())
        }

        when (response.status.value) {
            in 200..299 -> return response.body()
            else -> {
                val body = response.bodyAsText()
                error("Chrono API error (${response.status.value}): $body\n\nIs Chrono running at $baseUrl?")
            }
        }
    }

    fun close() {
        client.close()
    }
}
