package pro.dev.tt.api

import io.ktor.client.*
import io.ktor.client.call.*
import io.ktor.client.engine.cio.*
import io.ktor.client.plugins.contentnegotiation.*
import io.ktor.client.request.*
import io.ktor.client.statement.*
import io.ktor.http.*
import io.ktor.http.content.TextContent
import io.ktor.serialization.kotlinx.json.*
import kotlinx.serialization.SerializationStrategy
import kotlinx.serialization.json.Json
import pro.dev.tt.model.*
import java.util.UUID

class ApiException(val statusCode: Int, message: String) : Exception(message)

class AuthRequiredException : Exception("Authentication required")

class TtApiClient(private val cookie: String) {
    private val baseUrl = "https://timetrackingportal.dev.pro/api"

    private val json = Json {
        ignoreUnknownKeys = true
        isLenient = true
        encodeDefaults = true
    }

    private val client = HttpClient(CIO) {
        install(ContentNegotiation) {
            json(json)
        }
    }

    // Request bodies name their serializer at the call site instead of letting
    // Ktor look one up from the body's type. That lookup is reflective, so in
    // the native image the class must appear in reflect-config.json — and it
    // never can: the metadata comes from tracing `settle --dry-run`, a run that
    // by definition issues no POST, so no request class is ever reachable by the
    // agent. Hand-adding one would not survive either, since `metadataCopy`
    // overwrites that file. Naming the serializer makes it a compile-time
    // symbol, which no build step can lose. TextContent is an OutgoingContent,
    // so ContentNegotiation passes it through instead of re-encoding it.
    internal fun <T> jsonBody(serializer: SerializationStrategy<T>, value: T) =
        TextContent(json.encodeToString(serializer, value), ContentType.Application.Json)

    private suspend inline fun <reified T> HttpResponse.checkAndParse(): T {
        when (status.value) {
            401, 403 -> throw AuthRequiredException()
            404 -> throw ApiException(404, "Resource not found.")
            in 400..499 -> throw ApiException(status.value, "Client error: ${status.description}")
            in 500..599 -> throw ApiException(status.value, "Server error: ${status.description}")
        }
        return body()
    }

    private suspend fun HttpResponse.checkStatus(): Boolean {
        val responseBody = bodyAsText()
        when (status.value) {
            401, 403 -> throw AuthRequiredException()
            in 400..499 -> throw ApiException(status.value, "Client error (${status.value}): $responseBody")
            in 500..599 -> throw ApiException(status.value, "Server error (${status.value}): $responseBody")
        }
        return status == HttpStatusCode.OK
    }

    // The session cookie can only be refreshed by a host-side browser login
    // (`make auth`), which drives a GUI browser via Playwright and so must run
    // on the host. On 401/403 we surface a clear instruction instead of
    // attempting an in-process refresh.
    private suspend inline fun <reified T> withAuthCheck(block: () -> T): T {
        return try {
            block()
        } catch (e: AuthRequiredException) {
            println("\n❌ Dev.Pro Time Tracking Portal session expired or invalid.")
            println("   Run 'make auth' on your host machine to refresh the session.")
            throw ApiException(401, "Authentication failed. Session cookie expired — run 'make auth'.")
        }
    }

    suspend fun getCurrentUser(): CurrentUser = withAuthCheck {
        client.get("$baseUrl/contact/currentUser") {
            header("Cookie", cookie)
        }.checkAndParse()
    }

    suspend fun getAssignedProjects(contactId: String, dateFrom: String): AssignedProjectsResponse = withAuthCheck {
        client.get("$baseUrl/contact/$contactId/assignedProjectsOnDate") {
            header("Cookie", cookie)
            parameter("dateFrom", dateFrom)
        }.checkAndParse()
    }

    suspend fun getNormalView(period: String): NormalViewResponse = withAuthCheck {
        client.get("$baseUrl/timeTracking/normalView") {
            header("Cookie", cookie)
            parameter("period", period)
            parameter("pageInfo.pageIndex", 1)
            parameter("pageInfo.pageSize", 500)
        }.checkAndParse()
    }

    suspend fun createWorklog(request: CreateWorklogRequest): Boolean = withAuthCheck {
        client.post("$baseUrl/worklog/create") {
            header("Cookie", cookie)
            header("IdempotencyKey", UUID.randomUUID().toString())
            setBody(jsonBody(CreateWorklogRequest.serializer(), request))
        }.checkStatus()
    }

    suspend fun updateWorklog(request: UpdateWorklogRequest): Boolean = withAuthCheck {
        client.post("$baseUrl/worklog/update") {
            header("Cookie", cookie)
            header("IdempotencyKey", UUID.randomUUID().toString())
            setBody(jsonBody(UpdateWorklogRequest.serializer(), request))
        }.checkStatus()
    }

    suspend fun deleteWorklog(uniqueId: String): Boolean = withAuthCheck {
        client.delete("$baseUrl/worklog/$uniqueId") {
            header("Cookie", cookie)
        }.checkStatus()
    }

    fun close() {
        client.close()
    }
}
