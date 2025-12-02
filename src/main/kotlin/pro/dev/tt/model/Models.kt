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
    val isDeletable: Boolean = true
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
