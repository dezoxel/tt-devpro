package pro.dev.tt.service

import pro.dev.tt.config.Config
import pro.dev.tt.config.OverrideRule
import pro.dev.tt.model.ChronoTimeEntry
import pro.dev.tt.model.Project
import java.time.LocalDate

data class DayProjectAggregate(
    val date: LocalDate,
    val chronoProject: String,
    val totalHours: Double,
    val descriptions: List<String>,
    val devproProjectName: String,
    val billability: String
)

object Aggregator {

    fun aggregate(
        entries: List<ChronoTimeEntry>,
        config: Config,
        dateFrom: LocalDate? = null,
        dateTo: LocalDate? = null
    ): List<DayProjectAggregate> {
        val mappingByChronoProject = config.mappings.associateBy { it.chronoProject }

        // Group by date, project, AND description (only Work projects)
        // Only entries with same description are grouped together
        val grouped = entries
            .filter { it.project != null && it.duration != null && it.duration > 0 }
            .filter { it.project!!.name.endsWith("DevPro - Work") }
            .filter { entry ->
                val date = LocalDate.parse(entry.startTime.substring(0, 10))
                (dateFrom == null || date >= dateFrom) && (dateTo == null || date <= dateTo)
            }
            .groupBy { entry ->
                val date = LocalDate.parse(entry.startTime.substring(0, 10))
                val projectName = entry.project!!.name
                val description = entry.description?.trim() ?: ""
                Triple(date, projectName, description)
            }

        return grouped.map { (key, entriesGroup) ->
            val (date, chronoProject, description) = key

            // Check override rules first (by description pattern)
            val override = findOverride(description, config.overrides)

            val (devproProject, billability) = if (override != null) {
                override.devproProject to override.billability
            } else {
                val mapping = mappingByChronoProject[chronoProject]
                    ?: error(buildUnmappedProjectError(chronoProject, config))
                mapping.devproProject to mapping.billability
            }

            val totalSeconds = entriesGroup.sumOf { it.duration ?: 0 }
            val totalHours = totalSeconds / 3600.0

            DayProjectAggregate(
                date = date,
                chronoProject = chronoProject,
                totalHours = totalHours,
                descriptions = if (description.isNotBlank()) listOf(description) else emptyList(),
                devproProjectName = devproProject,
                billability = billability
            )
        }.sortedWith(compareBy({ it.date }, { it.devproProjectName }))
    }

    fun resolveProjectIds(
        aggregates: List<DayProjectAggregate>,
        devproProjects: List<Project>
    ): Map<String, String> {
        val projectByName = devproProjects.associateBy { it.shortName.lowercase() }
        val uniqueDevproNames = aggregates.map { it.devproProjectName }.distinct()

        return uniqueDevproNames.associateWith { name ->
            projectByName[name.lowercase()]?.uniqueId
                ?: error(buildDevproNotFoundError(name, devproProjects))
        }
    }

    private fun findOverride(description: String, overrides: List<OverrideRule>): OverrideRule? {
        if (description.isBlank()) return null
        return overrides.find { rule ->
            description.contains(rule.pattern, ignoreCase = true)
        }
    }

    private fun buildUnmappedProjectError(chronoProject: String, config: Config): String {
        val configured = config.mappings.map { it.chronoProject }
        return """
            |Chrono project '$chronoProject' has no mapping in config.
            |
            |Add to ~/.tt-config.yaml:
            |
            |mappings:
            |  - chrono_project: "$chronoProject"
            |    devpro_project: "YourDevProProjectName"
            |    billability: "Billable"
            |
            |Currently configured projects:
            |${configured.joinToString("\n") { "  - $it" }}
        """.trimMargin()
    }

    private fun buildDevproNotFoundError(name: String, available: List<Project>): String {
        val names = available.map { it.shortName }
        return """
            |DevPro project '$name' not found.
            |
            |Available projects:
            |${names.joinToString("\n") { "  - $it" }}
        """.trimMargin()
    }
}
