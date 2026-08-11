package pro.dev.tt

import pro.dev.tt.model.Project
import pro.dev.tt.service.Aggregator
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class ProjectIdResolutionTest {

    private val livePresales = Project(uniqueId = "live-presales", shortName = "Presales")
    private val liveVelocitor = Project(uniqueId = "live-velocitor", shortName = "Velocitor: NLP")
    private val liveProjects = listOf(livePresales, liveVelocitor)

    @Test
    fun `live list wins over a configured id for the same name`() {
        val resolution = Aggregator.resolveProjectIds(
            projectNames = listOf("Presales"),
            devproProjects = liveProjects,
            configuredIds = mapOf("Presales" to "stale-configured-id")
        )

        assertEquals(mapOf("Presales" to "live-presales"), resolution.idsByName)
        assertTrue(resolution.fallbacks.isEmpty(), "a live hit must not be reported as a fallback")
    }

    @Test
    fun `name missing from the live list falls back to the configured id`() {
        val resolution = Aggregator.resolveProjectIds(
            projectNames = listOf("Presales", "Inveniam SOW #3"),
            devproProjects = liveProjects,
            configuredIds = mapOf("Inveniam SOW #3" to "configured-inveniam")
        )

        assertEquals("configured-inveniam", resolution.idsByName["Inveniam SOW #3"])
        assertEquals("live-presales", resolution.idsByName["Presales"])
        assertEquals(1, resolution.fallbacks.size)
        assertEquals("Inveniam SOW #3", resolution.fallbacks.single().name)
        assertEquals("configured-inveniam", resolution.fallbacks.single().id)
    }

    @Test
    fun `name missing from both fails with the available projects listed`() {
        val error = assertFailsWith<IllegalStateException> {
            Aggregator.resolveProjectIds(
                projectNames = listOf("Ghost Project"),
                devproProjects = liveProjects,
                configuredIds = mapOf("Inveniam SOW #3" to "configured-inveniam")
            )
        }

        val message = error.message ?: ""
        assertTrue("Ghost Project" in message, "error names the unresolved project: $message")
        assertTrue("Presales" in message, "error lists available projects: $message")
        assertTrue("Velocitor: NLP" in message, "error lists available projects: $message")
        assertTrue("project_ids" in message, "error points at the config fallback: $message")
    }

    @Test
    fun `matching is case-insensitive on both the live list and the config keys`() {
        val resolution = Aggregator.resolveProjectIds(
            projectNames = listOf("presales", "INVENIAM SOW #3"),
            devproProjects = liveProjects,
            configuredIds = mapOf("Inveniam SOW #3" to "configured-inveniam")
        )

        assertEquals("live-presales", resolution.idsByName["presales"])
        assertEquals("configured-inveniam", resolution.idsByName["INVENIAM SOW #3"])
        assertEquals(listOf("INVENIAM SOW #3"), resolution.fallbacks.map { it.name })
    }

    @Test
    fun `fully live resolution reports no fallbacks`() {
        val resolution = Aggregator.resolveProjectIds(
            projectNames = listOf("Presales", "Velocitor: NLP", "Presales"),
            devproProjects = liveProjects,
            configuredIds = mapOf("Presales" to "stale-configured-id")
        )

        assertEquals(
            mapOf("Presales" to "live-presales", "Velocitor: NLP" to "live-velocitor"),
            resolution.idsByName
        )
        assertTrue(resolution.fallbacks.isEmpty())
    }

    @Test
    fun `an empty config leaves the previous behaviour intact`() {
        val resolution = Aggregator.resolveProjectIds(
            projectNames = listOf("Velocitor: NLP"),
            devproProjects = liveProjects,
            configuredIds = emptyMap()
        )

        assertEquals(mapOf("Velocitor: NLP" to "live-velocitor"), resolution.idsByName)
        assertTrue(resolution.fallbacks.isEmpty())
    }
}
