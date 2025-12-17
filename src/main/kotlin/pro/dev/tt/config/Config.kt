package pro.dev.tt.config

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class Config(
    @SerialName("chrono_api")
    val chronoApi: String,
    val mappings: List<ProjectMapping>,
    val fillers: List<Filler> = emptyList(),
    val overrides: List<OverrideRule> = emptyList()
)

@Serializable
data class ProjectMapping(
    @SerialName("chrono_project")
    val chronoProject: String,
    @SerialName("devpro_project")
    val devproProject: String,
    val billability: String
)

@Serializable
data class Filler(
    @SerialName("devpro_project")
    val devproProject: String,
    @SerialName("task_title")
    val taskTitle: String,
    val billability: String,
    @SerialName("min_hours")
    val minHours: Double,
    @SerialName("max_hours")
    val maxHours: Double
)

@Serializable
data class OverrideRule(
    val pattern: String,
    @SerialName("devpro_project")
    val devproProject: String,
    val billability: String
)
