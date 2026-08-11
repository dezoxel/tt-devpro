package pro.dev.tt

import pro.dev.tt.commands.renderAssignedProjects
import pro.dev.tt.model.Project
import java.time.LocalDate
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class AssignedProjectsRenderTest {

    private val presales = Project(uniqueId = "id-presales", shortName = "Presales")
    private val velocitor = Project(uniqueId = "id-velocitor", shortName = "Velocitor: NLP")

    @Test
    fun `header states the date the answer is scoped to`() {
        val output = renderAssignedProjects(LocalDate.of(2026, 8, 11), listOf(presales))

        assertTrue(
            output.lineSequence().first().contains("2026-08-11"),
            "the scoping date must be on the header line, got: $output"
        )
    }

    @Test
    fun `a date other than today renders that date, not today`() {
        val output = renderAssignedProjects(LocalDate.of(2025, 1, 1), listOf(presales))

        assertTrue(output.contains("2025-01-01"), "requested date missing, got: $output")
        assertTrue(
            !output.contains(LocalDate.now().toString()) || LocalDate.now() == LocalDate.of(2025, 1, 1),
            "today's date must not leak into an explicitly dated answer, got: $output"
        )
    }

    @Test
    fun `every project renders as shortName and uniqueId`() {
        val output = renderAssignedProjects(LocalDate.of(2026, 8, 11), listOf(presales, velocitor))

        assertTrue(output.contains("  Presales: id-presales"), "got: $output")
        assertTrue(output.contains("  Velocitor: NLP: id-velocitor"), "got: $output")
        assertEquals(3, output.lines().size, "header plus one line per project, got: $output")
    }

    @Test
    fun `header carries the project count`() {
        val output = renderAssignedProjects(LocalDate.of(2026, 8, 11), listOf(presales, velocitor))

        assertTrue(output.lineSequence().first().contains("(2)"), "count missing from header, got: $output")
    }

    @Test
    fun `an empty result says none instead of leaving a bare header`() {
        val output = renderAssignedProjects(LocalDate.of(2026, 8, 11), emptyList())

        assertTrue(output.contains("(none)"), "empty answer must be explicit, got: $output")
        assertTrue(output.contains("2026-08-11"), "empty answer still needs its date, got: $output")
    }
}
