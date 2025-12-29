package pro.dev.tt.model

import kotlinx.serialization.Serializable

@Serializable
data class Project(
    val uniqueId: String,
    val shortName: String,
    val isInternal: Boolean = false,
    val isFavorite: Boolean = false
)

@Serializable
data class AssignedProjectsResponse(
    val uniqueId: String,
    val projects: List<Project>
)

@Serializable
data class WorklogDetail(
    val uniqueId: String,
    val projectUniqueId: String,
    val projectShortName: String,
    val taskTitle: String,
    val billability: String,
    val loggedHours: Double,
    val isDeletable: Boolean = true,
    val expenseType: String? = null  // Added for testing
)

@Serializable
data class DateDetails(
    val date: String,
    val loggedHours: Double,
    val expectedHours: Double,
    val worklogsDetails: List<WorklogDetail> = emptyList()
)

@Serializable
data class PageItem(
    val contactUniqueId: String,
    val fullName: String,
    val loggedHours: Double,
    val expectedHours: Double,
    val detailsByDates: List<DateDetails> = emptyList()
)

@Serializable
data class NormalViewResponse(
    val totalLoggedHours: Double,
    val totalExpectedHours: Double,
    val pageList: List<PageItem>
)

@Serializable
data class CreateWorklogRequest(
    val worklogDate: String,
    val projectUniqueId: String,
    val taskTitle: String,
    val billability: String,
    val duration: Double,
    val description: String? = null,
    val overtime: Double? = null,
    val expenseType: String? = null,
    val pif: String? = null,
    val googleCalendarEventId: String? = null
)

@Serializable
data class UpdateWorklogRequest(
    val uniqueId: String,
    val worklogDate: String,
    val projectUniqueId: String,
    val taskTitle: String,
    val billability: String,
    val duration: Double,
    val description: String? = null,
    val overtime: Double? = null,
    val expenseType: String? = null,
    val pif: String? = null
)

@Serializable
data class CurrentUser(
    val uniqueId: String,
    val fullName: String,
    val email: String
)

// Chrono models

@Serializable
data class ChronoAspect(
    val id: Long,
    val name: String,
    val color: String
)

@Serializable
data class ChronoProject(
    val id: Long,
    val name: String,
    val color: String,
    val aspect: ChronoAspect? = null
)

@Serializable
data class ChronoTimeEntry(
    val id: Long,
    val description: String? = null,
    @kotlinx.serialization.SerialName("start_time")
    val startTime: String,
    @kotlinx.serialization.SerialName("end_time")
    val endTime: String? = null,
    val duration: Long? = null,  // seconds
    val project: ChronoProject? = null,
    val aspect: ChronoAspect? = null
)
