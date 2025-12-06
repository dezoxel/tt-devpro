package pro.dev.tt.config

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable
data class Config(
    @SerialName("chrono_api")
    val chronoApi: String,
    val mappings: List<ProjectMapping>
)

@Serializable
data class ProjectMapping(
    @SerialName("chrono_project")
    val chronoProject: String,
    @SerialName("devpro_project")
    val devproProject: String,
    val billability: String
)
