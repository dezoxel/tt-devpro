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
import java.util.UUID

class ApiException(val statusCode: Int, message: String) : Exception(message)

class TtApiClient(private val token: String) {
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
            401 -> throw ApiException(401, "Token expired or invalid. Please refresh your token.")
            403 -> throw ApiException(403, "Access denied.")
            404 -> throw ApiException(404, "Resource not found.")
            in 400..499 -> throw ApiException(status.value, "Client error: ${status.description}")
            in 500..599 -> throw ApiException(status.value, "Server error: ${status.description}")
        }
        return body()
    }

    private suspend fun HttpResponse.checkStatus(): Boolean {
        val responseBody = bodyAsText()
        when (status.value) {
            401 -> throw ApiException(401, "Token expired or invalid. Please refresh your token.")
            403 -> throw ApiException(403, "Access denied.")
            in 400..499 -> throw ApiException(status.value, "Client error (${status.value}): $responseBody")
            in 500..599 -> throw ApiException(status.value, "Server error (${status.value}): $responseBody")
        }
        return status == HttpStatusCode.OK
    }

    suspend fun getCurrentUser(): CurrentUser {
        return client.get("$baseUrl/contact/currentUser") {
            header("Authorization", "Bearer $token")
        }.checkAndParse()
    }

    suspend fun getAssignedProjects(contactId: String, dateFrom: String): AssignedProjectsResponse {
        return client.get("$baseUrl/contact/$contactId/assignedProjectsOnDate") {
            header("Authorization", "Bearer $token")
            parameter("dateFrom", dateFrom)
        }.checkAndParse()
    }

    suspend fun getNormalView(period: String): NormalViewResponse {
        return client.get("$baseUrl/timeTracking/normalView") {
            header("Authorization", "Bearer $token")
            parameter("period", period)
            parameter("pageInfo.pageIndex", 1)
            parameter("pageInfo.pageSize", 500)
        }.checkAndParse()
    }

    suspend fun createWorklog(request: CreateWorklogRequest): Boolean {
        return client.post("$baseUrl/worklog/create") {
            header("Authorization", "Bearer $token")
            header("IdempotencyKey", UUID.randomUUID().toString())
            contentType(ContentType.Application.Json)
            setBody(request)
        }.checkStatus()
    }

    suspend fun updateWorklog(request: UpdateWorklogRequest): Boolean {
        return client.post("$baseUrl/worklog/update") {
            header("Authorization", "Bearer $token")
            header("IdempotencyKey", UUID.randomUUID().toString())
            contentType(ContentType.Application.Json)
            setBody(request)
        }.checkStatus()
    }

    suspend fun deleteWorklog(uniqueId: String): Boolean {
        return client.delete("$baseUrl/worklog/$uniqueId") {
            header("Authorization", "Bearer $token")
        }.checkStatus()
    }

    fun close() {
        client.close()
    }
}
