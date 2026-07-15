package pro.dev.tt

import io.ktor.http.ContentType
import pro.dev.tt.api.TtApiClient
import pro.dev.tt.model.CreateWorklogRequest
import pro.dev.tt.model.UpdateWorklogRequest
import kotlin.test.AfterTest
import kotlin.test.Test
import kotlin.test.assertEquals

// Locks the body the portal actually receives on the write path. The write path
// has no metadata safety net — `settle --dry-run`, the run the native-image
// tracing agent records, never POSTs — so nothing but these assertions stands
// between a serialization slip and a settle that fails only in the native image.
class WorklogRequestBodyTest {

    private val client = TtApiClient("session=test")

    @AfterTest
    fun tearDown() = client.close()

    @Test
    fun `create worklog body is the raw json object the portal expects`() {
        val body = client.jsonBody(
            CreateWorklogRequest.serializer(),
            CreateWorklogRequest(
                worklogDate = "2026-07-13",
                projectUniqueId = "9096d358-040f-4a59-a623-8e5b1513b930",
                taskTitle = "Velocitor NLP Daily",
                billability = "Billable",
                duration = 0.5,
                expenseType = "None"
            )
        )

        assertEquals(ContentType.Application.Json, body.contentType)
        assertEquals(
            """{"worklogDate":"2026-07-13","projectUniqueId":"9096d358-040f-4a59-a623-8e5b1513b930",""" +
                """"taskTitle":"Velocitor NLP Daily","billability":"Billable","duration":0.5,""" +
                """"description":null,"overtime":null,"expenseType":"None","pif":null,""" +
                """"googleCalendarEventId":null}""",
            body.text
        )
    }

    @Test
    fun `update worklog body carries the worklog id`() {
        val body = client.jsonBody(
            UpdateWorklogRequest.serializer(),
            UpdateWorklogRequest(
                uniqueId = "0f7c2a1e-1111-2222-3333-444455556666",
                worklogDate = "2026-07-13",
                projectUniqueId = "9096d358-040f-4a59-a623-8e5b1513b930",
                taskTitle = "Velocitor NLP Daily",
                billability = "Billable",
                duration = 1.0,
                expenseType = "None"
            )
        )

        assertEquals(ContentType.Application.Json, body.contentType)
        assertEquals(
            """{"uniqueId":"0f7c2a1e-1111-2222-3333-444455556666","worklogDate":"2026-07-13",""" +
                """"projectUniqueId":"9096d358-040f-4a59-a623-8e5b1513b930","taskTitle":"Velocitor NLP Daily",""" +
                """"billability":"Billable","duration":1.0,"description":null,"overtime":null,""" +
                """"expenseType":"None","pif":null}""",
            body.text
        )
    }

    // A body Ktor re-encodes would arrive as a quoted JSON string, not an
    // object — the portal would take it and store nothing useful.
    @Test
    fun `body is not double-encoded`() {
        val body = client.jsonBody(
            CreateWorklogRequest.serializer(),
            CreateWorklogRequest(
                worklogDate = "2026-07-13",
                projectUniqueId = "p",
                taskTitle = "t",
                billability = "Billable",
                duration = 0.5
            )
        )

        assertEquals('{', body.text.first())
    }
}
