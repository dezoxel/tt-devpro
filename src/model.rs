//! Portal and Chrono payloads, plus the aggregate Kotlin declared inside the object that
//! produces it.
//!
//! Ports `model/Models.kt` and `model/LocalDateSerializer.kt`. `LocalDateSerializer`
//! has no Rust counterpart: `chrono::NaiveDate`'s own serde impl already encodes as
//! `YYYY-MM-DD`, which is what `LocalDate.toString()` produces.
//!
//! Serialization rules are not uniform across this file, and that is deliberate —
//! see C29. The write-path requests go out through a `Json { encodeDefaults = true }`
//! in Kotlin, so every absent optional is an explicit `null` on the wire and none of
//! them may carry `skip_serializing_if`.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Portal payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub unique_id: String,
    pub short_name: String,
    #[serde(default)]
    pub is_internal: bool,
    #[serde(default)]
    pub is_favorite: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignedProjectsResponse {
    pub unique_id: String,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorklogDetail {
    pub unique_id: String,
    pub project_unique_id: String,
    pub project_short_name: String,
    pub task_title: String,
    pub billability: String,
    pub logged_hours: f64,
    #[serde(default = "default_true")]
    pub is_deletable: bool,
    #[serde(default)]
    pub expense_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DateDetails {
    pub date: String,
    pub logged_hours: f64,
    pub expected_hours: f64,
    #[serde(default)]
    pub worklogs_details: Vec<WorklogDetail>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageItem {
    pub contact_unique_id: String,
    pub full_name: String,
    pub logged_hours: f64,
    pub expected_hours: f64,
    #[serde(default)]
    pub details_by_dates: Vec<DateDetails>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalViewResponse {
    pub total_logged_hours: f64,
    pub total_expected_hours: f64,
    pub page_list: Vec<PageItem>,
}

/// One billing period from `contact/ptrPeriods` — the portal's PTR Periods view. The
/// label is the only place the boundaries are given: `"October 01 - 15, 2026"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtrPeriod {
    pub ptr_period: String,
    pub expected_hours: f64,
}

/// C18: the field list and its order are the contract. Ends with `pif` and
/// `googleCalendarEventId`; carries five nulls on the settle path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorklogRequest {
    pub worklog_date: String,
    pub project_unique_id: String,
    pub task_title: String,
    pub billability: String,
    pub duration: f64,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub overtime: Option<f64>,
    #[serde(default)]
    pub expense_type: Option<String>,
    #[serde(default)]
    pub pif: Option<String>,
    #[serde(default)]
    pub google_calendar_event_id: Option<String>,
}

/// C18: leads with `uniqueId` and has **no** `googleCalendarEventId` — four nulls,
/// not five. The difference is not an oversight in the incumbent to tidy up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWorklogRequest {
    pub unique_id: String,
    pub worklog_date: String,
    pub project_unique_id: String,
    pub task_title: String,
    pub billability: String,
    pub duration: f64,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub overtime: Option<f64>,
    #[serde(default)]
    pub expense_type: Option<String>,
    #[serde(default)]
    pub pif: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentUser {
    pub unique_id: String,
    pub full_name: String,
    pub email: String,
}

// ---------------------------------------------------------------------------
// Chrono payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChronoAspect {
    pub id: i64,
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChronoProject {
    pub id: i64,
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub aspect: Option<ChronoAspect>,
}

/// No `rename_all` here on purpose: `Models.kt:111,113` pin `start_time` and
/// `end_time` with `@SerialName`, and every other key is already a single word,
/// so Rust's own snake_case field names reproduce the wire format exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChronoTimeEntry {
    pub id: i64,
    #[serde(default)]
    pub description: Option<String>,
    pub start_time: String,
    #[serde(default)]
    pub end_time: Option<String>,
    /// Seconds.
    #[serde(default)]
    pub duration: Option<i64>,
    #[serde(default)]
    pub project: Option<ChronoProject>,
    #[serde(default)]
    pub aspect: Option<ChronoAspect>,
}

// ---------------------------------------------------------------------------
// The type hoisted out of the aggregator
// ---------------------------------------------------------------------------

/// `Aggregator.kt:14-23`. Pure data with no tie to the aggregation logic, read by the
/// meeting probe as well as the plan context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayProjectAggregate {
    pub date: NaiveDate,
    pub chrono_project: String,
    pub total_hours: f64,
    pub descriptions: Vec<String>,
    pub devpro_project_name: String,
    pub billability: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hours: Option<f64>,
}

// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aggregate() -> DayProjectAggregate {
        DayProjectAggregate {
            date: NaiveDate::from_ymd_opt(2026, 9, 18).unwrap(),
            chrono_project: "Practices - DevPro - Work".to_string(),
            total_hours: 0.5,
            descriptions: vec!["AI Heads Sync".to_string()],
            devpro_project_name: "Delivery Practices".to_string(),
            billability: "NonBillable".to_string(),
            max_hours: None,
        }
    }

    /// `maxHours` defaults to null in `Aggregator.kt:22` and is absent from every
    /// aggregate in the capture.
    #[test]
    fn an_aggregate_omits_max_hours_when_unset() {
        let json = serde_json::to_value(aggregate()).expect("serialize");
        assert!(!json.as_object().unwrap().contains_key("maxHours"));
        assert_eq!(json.get("date"), Some(&serde_json::json!("2026-09-18")));
    }

    /// `Models.kt:111,113` pin these two keys with `@SerialName`; everything else
    /// on the Chrono side is a single word.
    #[test]
    fn a_chrono_entry_reads_snake_case_time_keys() {
        let entry: ChronoTimeEntry = serde_json::from_str(
            r#"{"id":1,"description":"x","start_time":"2026-09-18T09:00:00Z","end_time":null,"duration":1800}"#,
        )
        .expect("deserialize");
        assert_eq!(entry.start_time, "2026-09-18T09:00:00Z");
        assert_eq!(entry.duration, Some(1800));
        assert!(entry.project.is_none());
    }

    /// The write path runs through `Json { encodeDefaults = true }`, so an absent
    /// optional goes out as an explicit `null` (C18). No `skip_serializing_if` may
    /// creep onto these two structs.
    #[test]
    fn a_create_body_emits_its_nulls() {
        let body = CreateWorklogRequest {
            worklog_date: "2026-09-18".to_string(),
            project_unique_id: "p".to_string(),
            task_title: "t".to_string(),
            billability: "Billable".to_string(),
            duration: 0.5,
            description: None,
            overtime: None,
            expense_type: Some("None".to_string()),
            pif: None,
            google_calendar_event_id: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert_eq!(
            json,
            r#"{"worklogDate":"2026-09-18","projectUniqueId":"p","taskTitle":"t","billability":"Billable","duration":0.5,"description":null,"overtime":null,"expenseType":"None","pif":null,"googleCalendarEventId":null}"#
        );
    }

    /// C18's correction: the Update body leads with `uniqueId` and has no
    /// `googleCalendarEventId` at all.
    #[test]
    fn an_update_body_has_no_google_calendar_event_id() {
        let body = UpdateWorklogRequest {
            unique_id: "w".to_string(),
            worklog_date: "2026-09-18".to_string(),
            project_unique_id: "p".to_string(),
            task_title: "t".to_string(),
            billability: "Billable".to_string(),
            duration: 1.0,
            description: None,
            overtime: None,
            expense_type: Some("None".to_string()),
            pif: None,
        };
        let json = serde_json::to_string(&body).expect("serialize");
        assert!(!json.contains("googleCalendarEventId"));
        assert!(json.starts_with(r#"{"uniqueId":"w","worklogDate""#));
        assert!(
            json.contains(r#""duration":1.0"#),
            "unexpected shape: {json}"
        );
    }

    /// `isDeletable` defaults to true in `Models.kt:34`, unlike every other
    /// defaulted field on the portal side.
    #[test]
    fn a_worklog_detail_defaults_is_deletable_to_true() {
        let detail: WorklogDetail = serde_json::from_str(
            r#"{"uniqueId":"w","projectUniqueId":"p","projectShortName":"P","taskTitle":"t","billability":"Billable","loggedHours":1.0}"#,
        )
        .expect("deserialize");
        assert!(detail.is_deletable);
        assert_eq!(detail.expense_type, None);
    }

    /// The portal sends keys this tool does not model, and the incumbent's reader
    /// is configured with `ignoreUnknownKeys = true` — so leniency here is the
    /// contract, the exact opposite of `config.rs`.
    #[test]
    fn portal_payloads_ignore_unknown_keys() {
        let project: Project =
            serde_json::from_str(r#"{"uniqueId":"p","shortName":"P","somethingNew":42}"#)
                .expect("deserialize");
        assert_eq!(project.short_name, "P");
        assert!(!project.is_internal);
    }
}
