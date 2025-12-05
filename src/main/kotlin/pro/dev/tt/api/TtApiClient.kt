package pro.dev.tt.api

import io.ktor.client.*
import io.ktor.client.call.*
import io.ktor.client.engine.cio.*
import io.ktor.client.plugins.contentnegotiation.*
import io.ktor.client.request.*
import io.ktor.client.statement.*
import io.ktor.http.*
import io.ktor.serialization.kotlinx.json.*
import kotlinx.serialization.json.Json
import pro.dev.tt.model.*
import pro.dev.tt.service.BrowserAuthService
import java.util.UUID

class ApiException(val statusCode: Int, message: String) : Exception(message)

class TokenRefreshRequiredException : Exception("Token refresh required")

class TtApiClient(private var token: String, private val onTokenRefresh: ((String) -> Unit)? = null) {
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

    private suspend inline fun <reified T> HttpResponse.checkAndParse(): T {
        when (status.value) {
            401, 403 -> throw TokenRefreshRequiredException()
            404 -> throw ApiException(404, "Resource not found.")
            in 400..499 -> throw ApiException(status.value, "Client error: ${status.description}")
            in 500..599 -> throw ApiException(status.value, "Server error: ${status.description}")
        }
        return body()
    }

    private suspend fun HttpResponse.checkStatus(): Boolean {
        val responseBody = bodyAsText()
        when (status.value) {
            401, 403 -> throw TokenRefreshRequiredException()
            in 400..499 -> throw ApiException(status.value, "Client error (${status.value}): $responseBody")
            in 500..599 -> throw ApiException(status.value, "Server error (${status.value}): $responseBody")
        }
        return status == HttpStatusCode.OK
    }

    private fun isRunningInDocker(): Boolean {
        return java.io.File("/.dockerenv").exists() ||
               System.getenv("DOCKER_CONTAINER") != null
    }

    private fun refreshTokenIfNeeded(): Boolean {
        if (isRunningInDocker()) {
            println("\n❌ Token expired or invalid.")
            println("   Run ./auth.sh on your host machine to refresh the token.")
            return false
        }

        println("\n⚠️  Token expired or invalid. Refreshing token...")
        val newToken = BrowserAuthService.refreshTokenViaBrowser()

        if (newToken != null) {
            token = newToken
            BrowserAuthService.saveToken(newToken)
            onTokenRefresh?.invoke(newToken)
            return true
        }

        println("❌ Token refresh failed.")
        println("   Run ./auth.sh on your host machine to refresh the token.")
        return false
    }

    private suspend inline fun <reified T> withTokenRefresh(block: suspend () -> T): T {
        return try {
            block()
        } catch (e: TokenRefreshRequiredException) {
            if (refreshTokenIfNeeded()) {
                block() // Retry with new token
            } else {
                throw ApiException(401, "Authentication failed. Unable to refresh token.")
            }
        }
    }

    suspend fun getCurrentUser(): CurrentUser = withTokenRefresh {
        client.get("$baseUrl/contact/currentUser") {
            header("Authorization", "Bearer $token")
        }.checkAndParse()
    }

    suspend fun getAssignedProjects(contactId: String, dateFrom: String): AssignedProjectsResponse = withTokenRefresh {
        client.get("$baseUrl/contact/$contactId/assignedProjectsOnDate") {
            header("Authorization", "Bearer $token")
            parameter("dateFrom", dateFrom)
        }.checkAndParse()
    }

    suspend fun getNormalView(period: String): NormalViewResponse = withTokenRefresh {
        client.get("$baseUrl/timeTracking/normalView") {
            header("Authorization", "Bearer $token")
            parameter("period", period)
            parameter("pageInfo.pageIndex", 1)
            parameter("pageInfo.pageSize", 500)
        }.checkAndParse()
    }

    suspend fun createWorklog(request: CreateWorklogRequest): Boolean = withTokenRefresh {
        client.post("$baseUrl/worklog/create") {
            header("Authorization", "Bearer $token")
            header("IdempotencyKey", UUID.randomUUID().toString())
            contentType(ContentType.Application.Json)
            setBody(request)
        }.checkStatus()
    }

    suspend fun updateWorklog(request: UpdateWorklogRequest): Boolean = withTokenRefresh {
        client.post("$baseUrl/worklog/update") {
            header("Authorization", "Bearer $token")
            header("IdempotencyKey", UUID.randomUUID().toString())
            contentType(ContentType.Application.Json)
            setBody(request)
        }.checkStatus()
    }

    suspend fun deleteWorklog(uniqueId: String): Boolean = withTokenRefresh {
        client.delete("$baseUrl/worklog/$uniqueId") {
            header("Authorization", "Bearer $token")
        }.checkStatus()
    }

    fun close() {
        client.close()
    }
}
