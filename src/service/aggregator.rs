//! Day/project aggregation and DevPro project-id resolution.
//!
//! Ports `service/Aggregator.kt`. Carries four contracts outright and half of a
//! fifth:
//!
//! - **C2** — `start_time` is an RFC3339 instant and every use of it here is
//!   re-dated through the system zone to a *local* date, in two independent
//!   places: the `date_from`/`date_to` filter (`Aggregator.kt:49`) and the group
//!   key (`Aggregator.kt:53`). The Chrono fetch pads `+1 day` on the UTC axis so a late local
//!   evening stored under the next UTC day is still fetched; this re-dating is
//!   what puts it back on its own local day. It must never turn into a bound on
//!   the UTC date, and it must never turn into a bound the fetch applies —
//!   both places have to agree or a padded entry is fetched and then dropped.
//! - **C3** — `resolve_project_ids` is the single resolution point. The live
//!   assigned-projects list always wins; `project_ids` is consulted only on a
//!   miss; every fallback that fires is handed back so the caller can say on
//!   stderr which id came from config. A stale configured id silently beating a
//!   correct live one posts worklogs to the wrong project unnoticed, which is
//!   worse than the crash the fallback exists to prevent.
//! - **C9** — override rules are checked *before* the `chrono_project` mapping,
//!   and a matched override's `max_hours` caps the aggregate and marks it fixed
//!   for the normalizer.
//! - **C10** — only Chrono projects whose name ends in `DevPro - Work` or
//!   `DevPro/Work` are considered, and only entries with a positive duration.
//! - **C11** — the group key is (local date, chrono project, trimmed
//!   description), not (date, project).
//!
//! C28 applies throughout: Kotlin's `groupBy` and `associateBy` return
//! `LinkedHashMap`s and its `sortedWith` is a stable TimSort, so encounter order
//! survives both the grouping and the sort. A bare `HashMap` would randomize the
//! row order of two aggregates tying on `(date, devproProjectName)` and the order
//! of the C3 fallback warnings. Every ordered structure below is a `Vec` walked
//! in order for exactly that reason.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate, TimeZone};

use crate::config::{Config, OverrideRule};
use crate::fmt::utf16_cmp;
use crate::model::{ChronoTimeEntry, DayProjectAggregate, Project};

/// `Aggregator.kt:26`. A DevPro project name resolved from the config's
/// `project_ids` rather than the live list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackId {
    pub name: String,
    pub id: String,
}

/// `Aggregator.kt:28-31`.
///
/// `ids_by_name` is a plain `HashMap` and that is deliberate: Kotlin's
/// `mutableMapOf` is a `LinkedHashMap`, but its only consumers
/// (`SettleCommand.kt:500,518,545`, `projectIdMap[…]`) read it by key and never
/// iterate it, so no observable behaviour depends on the order.
/// `fallbacks` is the half that *is* iterated — `SettleCommand.kt:476` prints one
/// stderr warning per element — so it stays a `Vec` in encounter order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdResolution {
    pub ids_by_name: HashMap<String, String>,
    pub fallbacks: Vec<FallbackId>,
}

/// C2. `Aggregator.kt:49,53` — `Instant.parse(...).atZone(ZoneId.systemDefault()).toLocalDate()`.
///
/// `parse_from_rfc3339` rather than a fixed-format parse: the live Chrono data
/// carries both shapes, 1 729 values ending in `Z` and 956 with a numeric offset
/// over the 2026-07-01 → 2026-09-22 window, and `Instant.parse` accepts both.
pub fn entry_local_date<Tz: TimeZone>(start_time: &str, zone: &Tz) -> Result<NaiveDate> {
    let instant: DateTime<chrono::FixedOffset> = DateTime::parse_from_rfc3339(start_time)
        .with_context(|| format!("Could not parse Chrono start_time '{start_time}'"))?;
    Ok(instant.with_timezone(zone).date_naive())
}

/// `Aggregator.kt:35-88`, with the system zone.
pub fn aggregate(
    entries: &[ChronoTimeEntry],
    config: &Config,
    date_from: Option<NaiveDate>,
    date_to: Option<NaiveDate>,
) -> Result<Vec<DayProjectAggregate>> {
    aggregate_in_zone(entries, config, date_from, date_to, &Local)
}

/// The body of [`aggregate`] with the zone as a parameter.
///
/// Kotlin reads `ZoneId.systemDefault()` inline twice. Lifting it to a parameter
/// is the same move the plan sanctions for the normalizer's knowledge-base root
/// and the filler's RNG: production passes `Local` through [`aggregate`], and the
/// C2 tests get to assert a fixed offset instead of asserting whatever zone the
/// machine running them happens to sit in.
pub fn aggregate_in_zone<Tz: TimeZone>(
    entries: &[ChronoTimeEntry],
    config: &Config,
    date_from: Option<NaiveDate>,
    date_to: Option<NaiveDate>,
    zone: &Tz,
) -> Result<Vec<DayProjectAggregate>> {
    // `config.mappings.associateBy { it.chronoProject }` (`Aggregator.kt:41`).
    // `associateBy` puts without a guard, so a duplicated `chrono_project` keeps
    // the LAST mapping; read by key only, so a plain `insert` in order matches.
    let mut mapping_by_chrono_project: HashMap<&str, &crate::config::ProjectMapping> =
        HashMap::new();
    for mapping in &config.mappings {
        mapping_by_chrono_project.insert(mapping.chrono_project.as_str(), mapping);
    }

    // C11 grouping, in encounter order (`Aggregator.kt:45-57`). A `Vec` walked in
    // order rather than a map: `groupBy` returns a `LinkedHashMap` and the stable
    // sort at `Aggregator.kt:87` preserves that order on ties.
    type GroupKey = (NaiveDate, String, String);
    let mut groups: Vec<(GroupKey, Vec<&ChronoTimeEntry>)> = Vec::new();

    for entry in entries {
        // `Aggregator.kt:46` — project and a positive duration.
        let Some(project) = entry.project.as_ref() else {
            continue;
        };
        let Some(duration) = entry.duration else {
            continue;
        };
        if duration <= 0 {
            continue;
        }

        // C10, `Aggregator.kt:47`.
        if !(project.name.ends_with("DevPro - Work") || project.name.ends_with("DevPro/Work")) {
            continue;
        }

        // C2, first site — the range filter (`Aggregator.kt:48-51`).
        let date = entry_local_date(&entry.start_time, zone)?;
        // Spelled out rather than as a let-chain: `Cargo.toml` declares
        // `rust-version = "1.87"` and let-chains landed in 1.88.
        if date_from.is_some_and(|from| date < from) {
            continue;
        }
        if date_to.is_some_and(|to| date > to) {
            continue;
        }

        // C2, second site — the group key (`Aggregator.kt:52-57`). Re-derived
        // rather than reused in Kotlin; identical by construction here, which is
        // the property that matters.
        let description = entry
            .description
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();
        let key = (date, project.name.clone(), description);

        match groups.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, bucket)) => bucket.push(entry),
            None => groups.push((key, vec![entry])),
        }
    }

    let mut aggregates: Vec<DayProjectAggregate> = Vec::with_capacity(groups.len());

    for ((date, chrono_project, description), bucket) in groups {
        // C9 — overrides are consulted before the mapping (`Aggregator.kt:63`).
        let matched = find_override(&description, &config.overrides);

        let (devpro_project, billability, max_hours) = match matched {
            Some(rule) => (
                rule.devpro_project.clone(),
                rule.billability.clone(),
                rule.max_hours,
            ),
            None => match mapping_by_chrono_project.get(chrono_project.as_str()) {
                Some(mapping) => (
                    mapping.devpro_project.clone(),
                    mapping.billability.clone(),
                    None,
                ),
                // `Aggregator.kt:69` — `error(...)`, not a skip. The repo's own
                // "unmapped Chrono projects are silently skipped" note describes
                // C10's suffix filter, not this.
                None => bail!(unmapped_project_error(&chrono_project, config)),
            },
        };

        let total_seconds: i64 = bucket.iter().map(|e| e.duration.unwrap_or(0)).sum();
        let raw_hours = total_seconds as f64 / 3600.0;
        // C9's cap (`Aggregator.kt:76`). Strictly greater, so an entry landing
        // exactly on the cap keeps its own value.
        let total_hours = match max_hours {
            Some(cap) if raw_hours > cap => cap,
            _ => raw_hours,
        };

        aggregates.push(DayProjectAggregate {
            date,
            chrono_project,
            total_hours,
            // `Aggregator.kt:82`. At most one element — the key's description.
            descriptions: if description.is_empty() {
                Vec::new()
            } else {
                vec![description]
            },
            devpro_project_name: devpro_project,
            billability,
            max_hours,
        });
    }

    // `Aggregator.kt:87` — `sortedWith(compareBy(date, devproProjectName))`, a
    // stable TimSort. `sort_by`, never `sort_unstable_by`: ties keep the
    // encounter order the grouping above established (C28).
    aggregates.sort_by(|a, b| {
        a.date
            .cmp(&b.date)
            .then_with(|| utf16_cmp(&a.devpro_project_name, &b.devpro_project_name))
    });

    Ok(aggregates)
}

/// C3. `Aggregator.kt:100-126`.
///
/// The order of the two lookups is the contract. Reversing them would let a
/// stale `project_ids` entry beat a correct live one and post worklogs to the
/// wrong project with nothing on screen to say so.
pub fn resolve_project_ids(
    project_names: &[String],
    devpro_projects: &[Project],
    configured_ids: &HashMap<String, String>,
) -> Result<ProjectIdResolution> {
    // `Aggregator.kt:105` — `associateBy { it.shortName.lowercase() }`. Read out
    // of kotlin-stdlib's bytecode: `associateBy` builds a `LinkedHashMap` and
    // calls a plain `put` per element with the return value discarded, so two
    // projects whose short names differ only in case collapse onto one key and
    // the LATER one in the API's list wins. `entry().or_insert()` would keep the
    // first; a plain `insert` in list order reproduces it.
    let mut project_by_name: HashMap<String, &Project> = HashMap::new();
    for project in devpro_projects {
        project_by_name.insert(project.short_name.to_lowercase(), project);
    }

    // `Aggregator.kt:106`.
    let mut configured_by_name: HashMap<String, &str> = HashMap::new();
    for (key, value) in configured_ids {
        configured_by_name.insert(key.to_lowercase(), value.as_str());
    }

    let mut ids_by_name: HashMap<String, String> = HashMap::new();
    let mut fallbacks: Vec<FallbackId> = Vec::new();

    for name in distinct(project_names) {
        let lowered = name.to_lowercase();

        // Live list first, always (`Aggregator.kt:112-116`).
        if let Some(project) = project_by_name.get(&lowered) {
            ids_by_name.insert(name.clone(), project.unique_id.clone());
            continue;
        }

        // Config only on a miss (`Aggregator.kt:118-119`).
        let Some(configured_id) = configured_by_name.get(&lowered) else {
            bail!(devpro_not_found_error(name, devpro_projects));
        };

        ids_by_name.insert(name.clone(), (*configured_id).to_string());
        fallbacks.push(FallbackId {
            name: name.clone(),
            id: (*configured_id).to_string(),
        });
    }

    Ok(ProjectIdResolution {
        ids_by_name,
        fallbacks,
    })
}

/// `Aggregator.kt:111` — `projectNames.distinct()`, which keeps first-encounter
/// order. That order is what decides the order of the C3 fallback warnings on
/// stderr, and `Velocitor: NLP` and `Artory` both fall back on an ordinary run,
/// so it is reachable rather than theoretical (C28).
fn distinct(names: &[String]) -> Vec<&String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out: Vec<&String> = Vec::new();
    for name in names {
        if !seen.contains(&name.as_str()) {
            seen.push(name.as_str());
            out.push(name);
        }
    }
    out
}

/// `Aggregator.kt:128-133`. The FIRST rule whose pattern appears in the
/// description wins; a blank description matches nothing at all.
fn find_override<'a>(description: &str, overrides: &'a [OverrideRule]) -> Option<&'a OverrideRule> {
    // `isBlank()`, not `isEmpty()` — although the caller hands us an already
    // trimmed string, so the two coincide there.
    if description.chars().all(char::is_whitespace) {
        return None;
    }
    overrides
        .iter()
        .find(|rule| contains_ignore_case(description, &rule.pattern))
}

/// `String.contains(other, ignoreCase = true)`, which is `indexOf` over
/// `regionMatchesImpl` — a per-character comparison, not a whole-string
/// case fold.
///
/// `haystack.to_lowercase().contains(&needle.to_lowercase())` is the obvious
/// port, and it diverges wherever a full lowercase mapping expands one character
/// into several: `İ` U+0130 folds to two code points under Rust's full mapping
/// and to one under Java's simple one, so the folded haystack grows a character
/// the character-by-character comparison never sees and matches needles Kotlin
/// rejects. Comparing character by character cannot invent a character.
///
/// Residual divergences stay, and there are **82**, not the one this comment
/// first named. Measured by walking every code point on both sides —
/// `Character.toUpperCase`/`toLowerCase` on JDK 21 against the shipped helpers
/// on rustc 1.91 — and re-run 2026-09-22, which reproduced 82, 28 and 54
/// exactly (`~/.cache/tt-devpro-rewrite/measurements/casecmp/`, which carries
/// the probes and the command that produced them). They split cleanly in two,
/// and neither half is closable here:
///
/// - **28 are simple-vs-full gaps.** Java has a single-character mapping where
///   Rust's standard library exposes only the multi-character one, so the
///   fallback in [`simple_uppercase`] returns the input unchanged. `İ` U+0130
///   is one; the Greek ypogegrammeni block U+1F80..U+1FF3 is most of the rest.
/// - **54 are Unicode version skew.** rustc 1.91 knows case pairs this JDK does
///   not (U+A7CB..U+A7DC, U+10D50..U+10D85 and neighbours), so Rust folds where
///   the JVM does not.
///
/// Zero code points map differently on both sides, which is what says the
/// *mechanism* is right and only the tables differ. A hand-carried table would
/// close the first 28 and pin the second 54 to one JDK's Unicode version, which
/// moves under both toolchains — trading a named divergence for a hidden,
/// version-dependent one.
///
/// Not reachable on this tool's data: over the live 2 710-entry window
/// (2026-07-01..2026-09-22) the descriptions, projects and aspects contain 43
/// distinct non-ASCII characters and **none** of the 82; `~/.tt-config.yaml`
/// contains one non-ASCII character (`→`) and it is not among them either.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    // `indexOf` of an empty needle is 0, so an override rule with an empty
    // pattern matches every non-blank description.
    if needle.is_empty() {
        return true;
    }

    let hay: Vec<char> = haystack.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    if pat.len() > hay.len() {
        return false;
    }

    (0..=(hay.len() - pat.len())).any(|start| {
        pat.iter()
            .enumerate()
            .all(|(offset, &p)| chars_equal_ignore_case(hay[start + offset], p))
    })
}

/// `kotlin.text.Char.equals(other, ignoreCase = true)`: equal outright, or equal
/// uppercased, or equal after lowercasing *those uppercased forms* — the third
/// arm operates on the uppercased characters, not on the originals.
fn chars_equal_ignore_case(a: char, b: char) -> bool {
    if a == b {
        return true;
    }
    let a_upper = simple_uppercase(a);
    let b_upper = simple_uppercase(b);
    a_upper == b_upper || simple_lowercase(a_upper) == simple_lowercase(b_upper)
}

/// `Character.toUpperCase(char)` — a single character in, a single character
/// out. Rust's `char::to_uppercase` is the full mapping, which can yield several
/// characters (`ß` → `SS`); Java's single-char form returns the input unchanged
/// in exactly those cases.
fn simple_uppercase(c: char) -> char {
    let mut mapped = c.to_uppercase();
    match (mapped.next(), mapped.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// `Character.toLowerCase(char)`, the mirror of [`simple_uppercase`].
fn simple_lowercase(c: char) -> char {
    let mut mapped = c.to_lowercase();
    match (mapped.next(), mapped.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// `Aggregator.kt:135-150`, after `trimMargin()`.
fn unmapped_project_error(chrono_project: &str, config: &Config) -> String {
    let configured = config
        .mappings
        .iter()
        .map(|m| format!("  - {}", m.chrono_project))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Chrono project '{chrono_project}' has no mapping in config.\n\
         \n\
         Add to ~/.tt-config.yaml:\n\
         \n\
         mappings:\n\
         \x20 - chrono_project: \"{chrono_project}\"\n\
         \x20   devpro_project: \"YourDevProProjectName\"\n\
         \x20   billability: \"Billable\"\n\
         \n\
         Currently configured projects:\n\
         {configured}"
    )
}

/// `Aggregator.kt:152-166`, after `trimMargin()`.
fn devpro_not_found_error(name: &str, available: &[Project]) -> String {
    let names = available
        .iter()
        .map(|p| format!("  - {}", p.short_name))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "DevPro project '{name}' not found.\n\
         \n\
         Available projects:\n\
         {names}\n\
         \n\
         If the project was renamed or unassigned, either point the mapping at its\n\
         current name above, or record its id as a fallback in ~/.tt-config.yaml:\n\
         \n\
         project_ids:\n\
         \x20 \"{name}\": \"<uniqueId>\""
    )
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;
    use crate::config::ProjectMapping;
    use crate::model::ChronoProject;
    use chrono::FixedOffset;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// UTC-4, the offset the live data actually carries (`…-04:00`). Fixed so
    /// the C2 cases assert a date rather than asserting the machine's zone.
    fn edt() -> FixedOffset {
        FixedOffset::west_opt(4 * 3600).expect("UTC-4 is a valid offset")
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn entry(
        id: i64,
        start: &str,
        seconds: i64,
        project: &str,
        description: &str,
    ) -> ChronoTimeEntry {
        ChronoTimeEntry {
            id,
            description: Some(description.to_string()),
            start_time: start.to_string(),
            end_time: None,
            duration: Some(seconds),
            project: Some(ChronoProject {
                id: 1,
                name: project.to_string(),
                color: "#000".to_string(),
                aspect: None,
            }),
            aspect: None,
        }
    }

    fn mapping(chrono_project: &str, devpro_project: &str, billability: &str) -> ProjectMapping {
        ProjectMapping {
            chrono_project: chrono_project.to_string(),
            devpro_project: devpro_project.to_string(),
            billability: billability.to_string(),
        }
    }

    fn override_rule(pattern: &str, devpro_project: &str, max_hours: Option<f64>) -> OverrideRule {
        OverrideRule {
            pattern: pattern.to_string(),
            devpro_project: devpro_project.to_string(),
            billability: "NonBillable".to_string(),
            max_hours,
        }
    }

    fn config(mappings: Vec<ProjectMapping>, overrides: Vec<OverrideRule>) -> Config {
        Config {
            chrono_api: "http://localhost:9247".to_string(),
            mappings,
            fillers: Vec::new(),
            overrides,
            project_ids: HashMap::new(),
            max_synthetic_hours: 4.0,
            vault_path: "/vault".into(),
            session_cookie: None,
        }
    }

    /// The ordinary shape: one Work mapping, no overrides.
    fn practices_config() -> Config {
        config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            Vec::new(),
        )
    }

    fn run(entries: &[ChronoTimeEntry], config: &Config) -> Vec<DayProjectAggregate> {
        aggregate_in_zone(entries, config, None, None, &edt()).expect("aggregation should succeed")
    }

    fn live_projects() -> Vec<Project> {
        vec![
            Project {
                unique_id: "live-presales".to_string(),
                short_name: "Presales".to_string(),
                is_internal: false,
                is_favorite: false,
            },
            Project {
                unique_id: "live-velocitor".to_string(),
                short_name: "Velocitor: NLP".to_string(),
                is_internal: false,
                is_favorite: false,
            },
        ]
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn configured(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // =======================================================================
    // ProjectIdResolutionTest — the six ported verbatim
    // =======================================================================

    /// C3. `ProjectIdResolutionTest.kt:16-26` — "live list wins over a
    /// configured id for the same name".
    #[test]
    fn the_live_list_wins_over_a_configured_id_for_the_same_name() {
        let resolution = resolve_project_ids(
            &names(&["Presales"]),
            &live_projects(),
            &configured(&[("Presales", "stale-configured-id")]),
        )
        .expect("resolution should succeed");

        assert_eq!(
            resolution.ids_by_name,
            configured(&[("Presales", "live-presales")])
        );
        assert!(
            resolution.fallbacks.is_empty(),
            "a live hit must not be reported as a fallback"
        );
    }

    /// C3. `ProjectIdResolutionTest.kt:28-41` — "name missing from the live list
    /// falls back to the configured id".
    #[test]
    fn a_name_missing_from_the_live_list_falls_back_to_the_configured_id() {
        let resolution = resolve_project_ids(
            &names(&["Presales", "Inveniam SOW #3"]),
            &live_projects(),
            &configured(&[("Inveniam SOW #3", "configured-inveniam")]),
        )
        .expect("resolution should succeed");

        assert_eq!(
            resolution
                .ids_by_name
                .get("Inveniam SOW #3")
                .map(String::as_str),
            Some("configured-inveniam")
        );
        assert_eq!(
            resolution.ids_by_name.get("Presales").map(String::as_str),
            Some("live-presales")
        );
        assert_eq!(resolution.fallbacks.len(), 1);
        assert_eq!(resolution.fallbacks[0].name, "Inveniam SOW #3");
        assert_eq!(resolution.fallbacks[0].id, "configured-inveniam");
    }

    /// C3. `ProjectIdResolutionTest.kt:43-58` — "name missing from both fails
    /// with the available projects listed".
    #[test]
    fn a_name_missing_from_both_fails_with_the_available_projects_listed() {
        let error = resolve_project_ids(
            &names(&["Ghost Project"]),
            &live_projects(),
            &configured(&[("Inveniam SOW #3", "configured-inveniam")]),
        )
        .expect_err("an unresolvable name must be an error");

        let message = error.to_string();
        assert!(
            message.contains("Ghost Project"),
            "error names the unresolved project: {message}"
        );
        assert!(
            message.contains("Presales"),
            "error lists available projects: {message}"
        );
        assert!(
            message.contains("Velocitor: NLP"),
            "error lists available projects: {message}"
        );
        assert!(
            message.contains("project_ids"),
            "error points at the config fallback: {message}"
        );
    }

    /// C3. `ProjectIdResolutionTest.kt:60-71` — "matching is case-insensitive on
    /// both the live list and the config keys".
    #[test]
    fn resolution_is_case_insensitive_on_both_the_live_list_and_the_config_keys() {
        let resolution = resolve_project_ids(
            &names(&["presales", "INVENIAM SOW #3"]),
            &live_projects(),
            &configured(&[("Inveniam SOW #3", "configured-inveniam")]),
        )
        .expect("resolution should succeed");

        assert_eq!(
            resolution.ids_by_name.get("presales").map(String::as_str),
            Some("live-presales")
        );
        assert_eq!(
            resolution
                .ids_by_name
                .get("INVENIAM SOW #3")
                .map(String::as_str),
            Some("configured-inveniam")
        );
        assert_eq!(
            resolution
                .fallbacks
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["INVENIAM SOW #3"]
        );
    }

    /// C3. `ProjectIdResolutionTest.kt:73-86` — "fully live resolution reports no
    /// fallbacks". Also pins `distinct()`: `Presales` appears twice and the
    /// resolved map holds two entries.
    #[test]
    fn a_fully_live_resolution_reports_no_fallbacks() {
        let resolution = resolve_project_ids(
            &names(&["Presales", "Velocitor: NLP", "Presales"]),
            &live_projects(),
            &configured(&[("Presales", "stale-configured-id")]),
        )
        .expect("resolution should succeed");

        assert_eq!(
            resolution.ids_by_name,
            configured(&[
                ("Presales", "live-presales"),
                ("Velocitor: NLP", "live-velocitor"),
            ])
        );
        assert!(resolution.fallbacks.is_empty());
    }

    /// C3. `ProjectIdResolutionTest.kt:88-98` — "an empty config leaves the
    /// previous behaviour intact".
    #[test]
    fn an_empty_config_leaves_the_previous_behaviour_intact() {
        let resolution = resolve_project_ids(
            &names(&["Velocitor: NLP"]),
            &live_projects(),
            &HashMap::new(),
        )
        .expect("resolution should succeed");

        assert_eq!(
            resolution.ids_by_name,
            configured(&[("Velocitor: NLP", "live-velocitor")])
        );
        assert!(resolution.fallbacks.is_empty());
    }

    // =======================================================================
    // C3 — beyond the ported six
    // =======================================================================

    /// C28 / `Aggregator.kt:105`. `associateBy` puts without a guard, so two
    /// portal projects whose short names differ only in case collapse onto one
    /// key and the LATER one in the API's list wins. `entry().or_insert()` — the
    /// idiomatic Rust reach — keeps the first and would fail this.
    #[test]
    fn a_short_name_case_collision_in_the_live_list_resolves_to_the_later_project() {
        let projects = vec![
            Project {
                unique_id: "first-in-the-list".to_string(),
                short_name: "Artory".to_string(),
                is_internal: false,
                is_favorite: false,
            },
            Project {
                unique_id: "later-in-the-list".to_string(),
                short_name: "ARTORY".to_string(),
                is_internal: false,
                is_favorite: false,
            },
        ];

        let resolution = resolve_project_ids(&names(&["Artory"]), &projects, &HashMap::new())
            .expect("resolution should succeed");

        assert_eq!(
            resolution.ids_by_name.get("Artory").map(String::as_str),
            Some("later-in-the-list"),
            "associateBy is last-wins on a key collision"
        );
        assert!(resolution.fallbacks.is_empty());
    }

    /// The other half of the same measurement: the colliding key keeps the first
    /// element's *position* in Kotlin's `LinkedHashMap`. Nothing observable
    /// depends on it here because the map is only ever read by key — asserted
    /// rather than asserted away, so that a future change making it iterable
    /// trips over this note.
    #[test]
    fn a_short_name_case_collision_still_resolves_under_either_spelling() {
        let projects = vec![
            Project {
                unique_id: "first-in-the-list".to_string(),
                short_name: "Artory".to_string(),
                is_internal: false,
                is_favorite: false,
            },
            Project {
                unique_id: "later-in-the-list".to_string(),
                short_name: "ARTORY".to_string(),
                is_internal: false,
                is_favorite: false,
            },
        ];

        let resolution = resolve_project_ids(
            &names(&["Artory", "ARTORY", "artory"]),
            &projects,
            &HashMap::new(),
        )
        .expect("resolution should succeed");

        for spelling in ["Artory", "ARTORY", "artory"] {
            assert_eq!(
                resolution.ids_by_name.get(spelling).map(String::as_str),
                Some("later-in-the-list"),
                "every spelling reaches the one collapsed key"
            );
        }
    }

    /// C28 / `Aggregator.kt:111`. `distinct()` keeps first-encounter order, and
    /// that order is what the stderr fallback warnings come out in.
    #[test]
    fn fallback_warnings_follow_the_encounter_order_of_the_project_names() {
        let resolution = resolve_project_ids(
            &names(&["Artory", "Presales", "Velocitor: NLP", "Artory"]),
            &live_projects(),
            &configured(&[
                ("Artory", "configured-artory"),
                ("Velocitor: NLP", "configured-velocitor"),
            ]),
        )
        .expect("resolution should succeed");

        // `Velocitor: NLP` is in the live list, so only `Artory` falls back —
        // and the configured id for `Velocitor: NLP` must lose to the live one.
        assert_eq!(
            resolution
                .fallbacks
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Artory"]
        );
        assert_eq!(
            resolution
                .ids_by_name
                .get("Velocitor: NLP")
                .map(String::as_str),
            Some("live-velocitor"),
            "a configured id must never override a live one"
        );
    }

    /// C3, the ordering claim stated as its own case: with the same name present
    /// in both sources the live id is used AND no warning is raised. A port that
    /// consulted the config first would still produce an id and still resolve,
    /// so this is the assertion that separates the two orders.
    #[test]
    fn a_stale_configured_id_never_beats_a_live_one_and_raises_no_warning() {
        let resolution = resolve_project_ids(
            &names(&["Presales", "Velocitor: NLP"]),
            &live_projects(),
            &configured(&[
                ("Presales", "stale-presales"),
                ("velocitor: nlp", "stale-velocitor"),
            ]),
        )
        .expect("resolution should succeed");

        assert_eq!(
            resolution.ids_by_name.get("Presales").map(String::as_str),
            Some("live-presales")
        );
        assert_eq!(
            resolution
                .ids_by_name
                .get("Velocitor: NLP")
                .map(String::as_str),
            Some("live-velocitor")
        );
        assert!(resolution.fallbacks.is_empty());
    }

    /// C3. Two names missing from the live list both fall back, in order, and
    /// both are reported — one warning per fallback, not one per run.
    #[test]
    fn every_fallback_that_fires_is_reported_not_just_the_first() {
        let resolution = resolve_project_ids(
            &names(&["Velocitor: NLP", "Artory", "Inveniam SOW #3"]),
            &[],
            &configured(&[
                ("Velocitor: NLP", "cfg-velocitor"),
                ("Artory", "cfg-artory"),
                ("Inveniam SOW #3", "cfg-inveniam"),
            ]),
        )
        .expect("resolution should succeed");

        assert_eq!(
            resolution
                .fallbacks
                .iter()
                .map(|f| (f.name.as_str(), f.id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Velocitor: NLP", "cfg-velocitor"),
                ("Artory", "cfg-artory"),
                ("Inveniam SOW #3", "cfg-inveniam"),
            ]
        );
    }

    /// `Aggregator.kt:152-166` verbatim, after `trimMargin()`. The message is the
    /// operator's only instruction when the portal stops listing a project, so
    /// its text is the contract rather than decoration.
    #[test]
    fn the_unresolvable_name_error_reads_exactly_as_the_incumbent_wrote_it() {
        let error = resolve_project_ids(
            &names(&["Ghost Project"]),
            &live_projects(),
            &HashMap::new(),
        )
        .expect_err("an unresolvable name must be an error");

        assert_eq!(
            error.to_string(),
            "DevPro project 'Ghost Project' not found.\n\
             \n\
             Available projects:\n\
             \x20 - Presales\n\
             \x20 - Velocitor: NLP\n\
             \n\
             If the project was renamed or unassigned, either point the mapping at its\n\
             current name above, or record its id as a fallback in ~/.tt-config.yaml:\n\
             \n\
             project_ids:\n\
             \x20 \"Ghost Project\": \"<uniqueId>\""
        );
    }

    /// The empty-live-list shape of the same message: `joinToString` over an
    /// empty list yields an empty line, not a missing one.
    #[test]
    fn the_unresolvable_name_error_keeps_its_blank_line_when_no_projects_are_assigned() {
        let error = resolve_project_ids(&names(&["Ghost"]), &[], &HashMap::new())
            .expect_err("an unresolvable name must be an error");

        assert!(
            error
                .to_string()
                .contains("Available projects:\n\n\nIf the project"),
            "unexpected message: {}",
            error
        );
    }

    /// An empty name list resolves to an empty map rather than erroring.
    #[test]
    fn no_project_names_resolve_to_an_empty_map() {
        let resolution = resolve_project_ids(&[], &live_projects(), &HashMap::new())
            .expect("resolution should succeed");
        assert!(resolution.ids_by_name.is_empty());
        assert!(resolution.fallbacks.is_empty());
    }

    /// A repeated name that falls back is warned about once, not twice —
    /// `distinct()` runs before the loop, not inside it.
    #[test]
    fn a_repeated_fallback_name_warns_once() {
        let resolution = resolve_project_ids(
            &names(&["Artory", "Artory", "Artory"]),
            &live_projects(),
            &configured(&[("Artory", "cfg-artory")]),
        )
        .expect("resolution should succeed");

        assert_eq!(resolution.fallbacks.len(), 1);
    }

    // =======================================================================
    // C10 — the project-name filter and the duration filter
    // =======================================================================

    /// C10. `Aggregator.kt:47` — the flat `Project - Parent - Work` form.
    #[test]
    fn a_flat_devpro_work_project_name_is_aggregated() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "Docs",
        )];
        let result = run(&entries, &practices_config());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].chrono_project, "Practices - DevPro - Work");
        assert_eq!(result[0].devpro_project_name, "Delivery Practices");
        assert_eq!(result[0].billability, "NonBillable");
    }

    /// C10. The hierarchical `Project/Parent/Work` form is the second accepted
    /// suffix, and it needs its own mapping key.
    #[test]
    fn a_hierarchical_devpro_work_project_name_is_aggregated() {
        let config = config(
            vec![mapping(
                "Practices/DevPro/Work",
                "Delivery Practices",
                "NonBillable",
            )],
            Vec::new(),
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices/DevPro/Work",
            "Docs",
        )];
        let result = run(&entries, &config);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].chrono_project, "Practices/DevPro/Work");
    }

    /// C10. A project on neither suffix is dropped **silently** — no error, no
    /// warning, nothing in the result. This is the documented "unmapped Chrono
    /// projects are silently skipped" behaviour, and silence is the contract: the
    /// config has a mapping for it and it still must not appear.
    #[test]
    fn a_project_on_neither_suffix_is_dropped_without_a_word() {
        let config = config(
            vec![
                mapping(
                    "Practices - DevPro - Work",
                    "Delivery Practices",
                    "NonBillable",
                ),
                mapping(
                    "Reading - DevPro - Personal",
                    "Delivery Practices",
                    "NonBillable",
                ),
            ],
            Vec::new(),
        );
        let entries = vec![
            entry(
                1,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Docs",
            ),
            entry(
                2,
                "2026-09-18T15:00:00Z",
                3600,
                "Reading - DevPro - Personal",
                "Book",
            ),
        ];

        let result = aggregate_in_zone(&entries, &config, None, None, &edt())
            .expect("a skipped project is not an error");

        assert_eq!(result.len(), 1, "only the Work entry survives");
        assert_eq!(result[0].chrono_project, "Practices - DevPro - Work");
    }

    /// C10. The suffix test is `endsWith`, so a name that merely *contains* the
    /// marker is not enough. Separates `ends_with` from `contains`.
    #[test]
    fn a_project_containing_but_not_ending_with_the_marker_is_dropped() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work - Archive",
            "Docs",
        )];
        assert!(run(&entries, &practices_config()).is_empty());
    }

    /// C10. `duration > 0` — a zero-duration entry is dropped, so a running or
    /// mis-saved Chrono entry cannot create an aggregate.
    #[test]
    fn a_zero_duration_entry_is_dropped() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            0,
            "Practices - DevPro - Work",
            "Docs",
        )];
        assert!(run(&entries, &practices_config()).is_empty());
    }

    /// C10. Strictly greater than zero, so a negative duration goes the same way.
    #[test]
    fn a_negative_duration_entry_is_dropped() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            -60,
            "Practices - DevPro - Work",
            "Docs",
        )];
        assert!(run(&entries, &practices_config()).is_empty());
    }

    /// C10. `duration != null`. The live window holds zero nulls, so this case is
    /// synthetic by construction — the Kotlin model says the field is nullable and
    /// a parity port does not narrow the type on the strength of one sample.
    #[test]
    fn an_entry_without_a_duration_is_dropped() {
        let mut e = entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "Docs",
        );
        e.duration = None;
        assert!(run(&[e], &practices_config()).is_empty());
    }

    /// C10. `project != null`, and the null-project branch is checked before the
    /// suffix test, so it cannot panic on the missing name.
    #[test]
    fn an_entry_without_a_project_is_dropped() {
        let mut e = entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "Docs",
        );
        e.project = None;
        assert!(run(&[e], &practices_config()).is_empty());
    }

    /// `Aggregator.kt:69`. A Chrono project that *passes* the C10 suffix filter
    /// but has no mapping is a hard error, not a silent skip — the opposite of
    /// the case two tests above, and the distinction the repo's one-line note
    /// about "unmapped projects are silently skipped" blurs.
    #[test]
    fn a_work_project_with_no_mapping_is_a_hard_error_listing_the_configured_ones() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Velocitor - DevPro - Work",
            "NLP",
        )];
        let error = aggregate_in_zone(&entries, &practices_config(), None, None, &edt())
            .expect_err("an unmapped Work project must be an error");

        assert_eq!(
            error.to_string(),
            "Chrono project 'Velocitor - DevPro - Work' has no mapping in config.\n\
             \n\
             Add to ~/.tt-config.yaml:\n\
             \n\
             mappings:\n\
             \x20 - chrono_project: \"Velocitor - DevPro - Work\"\n\
             \x20   devpro_project: \"YourDevProProjectName\"\n\
             \x20   billability: \"Billable\"\n\
             \n\
             Currently configured projects:\n\
             \x20 - Practices - DevPro - Work"
        );
    }

    /// The mapping lookup is exact, not case-insensitive — unlike the project-id
    /// resolution above. The two behaviours sit twenty lines apart in the same
    /// file and a port that unified them would pass every other test here.
    #[test]
    fn the_chrono_project_mapping_lookup_is_case_sensitive() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "practices - DevPro - Work",
            "Docs",
        )];
        assert!(
            aggregate_in_zone(&entries, &practices_config(), None, None, &edt()).is_err(),
            "a differently-cased chrono_project must not match the mapping"
        );
    }

    /// `associateBy` again, this time on `Aggregator.kt:41`: two mappings for one
    /// `chrono_project` collapse and the LAST one wins.
    #[test]
    fn a_duplicated_chrono_project_mapping_resolves_to_the_last_one() {
        let config = config(
            vec![
                mapping("Practices - DevPro - Work", "First Project", "Billable"),
                mapping("Practices - DevPro - Work", "Second Project", "NonBillable"),
            ],
            Vec::new(),
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "Docs",
        )];
        let result = run(&entries, &config);
        assert_eq!(result[0].devpro_project_name, "Second Project");
        assert_eq!(result[0].billability, "NonBillable");
    }

    // =======================================================================
    // C11 — the three-part grouping key
    // =======================================================================

    /// C11. Two entries, same day, same Chrono project, different descriptions →
    /// two aggregates. A (date, project) key would give one.
    #[test]
    fn two_descriptions_on_one_project_and_day_stay_two_aggregates() {
        let entries = vec![
            entry(
                1,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Docs",
            ),
            entry(
                2,
                "2026-09-18T15:00:00Z",
                1800,
                "Practices - DevPro - Work",
                "Review",
            ),
        ];
        let result = run(&entries, &practices_config());

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].descriptions, vec!["Docs".to_string()]);
        assert_eq!(result[0].total_hours, 1.0);
        assert_eq!(result[1].descriptions, vec!["Review".to_string()]);
        assert_eq!(result[1].total_hours, 0.5);
    }

    /// C11. The same description twice merges and the durations sum.
    #[test]
    fn two_entries_sharing_a_description_merge_and_sum_their_hours() {
        let entries = vec![
            entry(
                1,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Docs",
            ),
            entry(
                2,
                "2026-09-18T15:00:00Z",
                1800,
                "Practices - DevPro - Work",
                "Docs",
            ),
        ];
        let result = run(&entries, &practices_config());

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].total_hours, 1.5);
        assert_eq!(
            result[0].descriptions,
            vec!["Docs".to_string()],
            "descriptions holds at most the one key description"
        );
    }

    /// C11. The description in the key is *trimmed*, so two entries differing
    /// only in surrounding whitespace land in one group — and the stored
    /// description is the trimmed form.
    #[test]
    fn descriptions_are_trimmed_before_they_become_part_of_the_key() {
        let entries = vec![
            entry(
                1,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "  Docs  ",
            ),
            entry(
                2,
                "2026-09-18T15:00:00Z",
                1800,
                "Practices - DevPro - Work",
                "Docs",
            ),
        ];
        let result = run(&entries, &practices_config());

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].descriptions, vec!["Docs".to_string()]);
        assert_eq!(result[0].total_hours, 1.5);
    }

    /// C11. A blank description gives an empty `descriptions` list rather than a
    /// list holding an empty string — which is what makes the task title fall
    /// back to "Development work" downstream (C12).
    #[test]
    fn a_blank_description_yields_an_empty_descriptions_list() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "   ",
        )];
        let result = run(&entries, &practices_config());

        assert_eq!(result.len(), 1);
        assert!(result[0].descriptions.is_empty());
    }

    /// C11. A missing description behaves exactly as a blank one — `?: ""`.
    #[test]
    fn a_null_description_groups_with_a_blank_one() {
        let mut without = entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "",
        );
        without.description = None;
        let blank = entry(
            2,
            "2026-09-18T15:00:00Z",
            1800,
            "Practices - DevPro - Work",
            "  ",
        );

        let result = run(&[without, blank], &practices_config());

        assert_eq!(result.len(), 1, "null and blank share the empty-string key");
        assert_eq!(result[0].total_hours, 1.5);
        assert!(result[0].descriptions.is_empty());
    }

    /// C11. Two Chrono projects on the same day with the same description stay
    /// separate — the project is part of the key too.
    #[test]
    fn the_same_description_on_two_projects_stays_two_aggregates() {
        let config = config(
            vec![
                mapping(
                    "Practices - DevPro - Work",
                    "Delivery Practices",
                    "NonBillable",
                ),
                mapping("Presales - DevPro - Work", "Presales", "NonBillable"),
            ],
            Vec::new(),
        );
        let entries = vec![
            entry(
                1,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Sync",
            ),
            entry(
                2,
                "2026-09-18T15:00:00Z",
                3600,
                "Presales - DevPro - Work",
                "Sync",
            ),
        ];
        let result = run(&entries, &config);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].devpro_project_name, "Delivery Practices");
        assert_eq!(result[1].devpro_project_name, "Presales");
    }

    /// C11. The same project and description on two days stay separate — the date
    /// is part of the key.
    #[test]
    fn the_same_project_and_description_on_two_days_stay_two_aggregates() {
        let entries = vec![
            entry(
                1,
                "2026-09-17T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Docs",
            ),
            entry(
                2,
                "2026-09-18T13:00:00Z",
                1800,
                "Practices - DevPro - Work",
                "Docs",
            ),
        ];
        let result = run(&entries, &practices_config());

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].date, day(2026, 9, 17));
        assert_eq!(result[1].date, day(2026, 9, 18));
    }

    // =======================================================================
    // C2 — re-dating to the local day
    // =======================================================================

    /// C2. `Aggregator.kt:49,53`. An entry at 19:45 local on the 18th is stored
    /// as `2026-09-18T23:45:00Z` — UTC date 18, still the 18th locally. Its
    /// sibling at 20:15 local crosses into UTC the 19th and must still land on
    /// the 18th. Both under one group key, because the grouping re-dates too.
    #[test]
    fn an_entry_whose_utc_date_is_the_next_day_lands_on_its_local_day() {
        let entries = vec![
            entry(
                1,
                "2026-09-18T23:45:00Z",
                900,
                "Practices - DevPro - Work",
                "Late",
            ),
            entry(
                2,
                "2026-09-19T00:15:00Z",
                900,
                "Practices - DevPro - Work",
                "Late",
            ),
        ];
        let result = run(&entries, &practices_config());

        assert_eq!(result.len(), 1, "both re-date onto the same local day");
        assert_eq!(result[0].date, day(2026, 9, 18));
        assert_eq!(result[0].total_hours, 0.5);
    }

    /// C2, the filter half stated on its own. `--from`/`--to` are local dates, so
    /// the padded `+1 day` entry the Chrono fetch pulled in must survive a range
    /// that names only the 18th. A port filtering on the UTC date would drop it,
    /// and the padding would then be pointless.
    #[test]
    fn the_range_filter_bounds_the_local_date_not_the_utc_one() {
        let entries = vec![entry(
            1,
            "2026-09-19T01:30:00Z",
            3600,
            "Practices - DevPro - Work",
            "Late",
        )];

        let result = aggregate_in_zone(
            &entries,
            &practices_config(),
            Some(day(2026, 9, 18)),
            Some(day(2026, 9, 18)),
            &edt(),
        )
        .expect("aggregation should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].date, day(2026, 9, 18));
    }

    /// C2. The mirror case: an entry whose UTC date is inside the range but whose
    /// local date is before it is dropped. Together with the test above this pins
    /// the axis rather than just the direction.
    #[test]
    fn an_entry_whose_local_date_falls_before_the_range_is_dropped() {
        let entries = vec![entry(
            1,
            "2026-09-18T02:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "Late",
        )];

        let result = aggregate_in_zone(
            &entries,
            &practices_config(),
            Some(day(2026, 9, 18)),
            None,
            &edt(),
        )
        .expect("aggregation should succeed");

        assert!(
            result.is_empty(),
            "22:00 local on the 17th is not inside a range starting on the 18th"
        );
    }

    /// C2, boundary. Exactly midnight local belongs to the day that starts there.
    #[test]
    fn an_entry_at_exactly_local_midnight_belongs_to_the_starting_day() {
        let entries = vec![entry(
            1,
            "2026-09-18T00:00:00-04:00",
            3600,
            "Practices - DevPro - Work",
            "Early",
        )];
        let result = run(&entries, &practices_config());
        assert_eq!(result[0].date, day(2026, 9, 18));
    }

    /// C2, the other boundary. One second before local midnight is still that day.
    #[test]
    fn an_entry_at_local_one_second_to_midnight_belongs_to_the_ending_day() {
        let entries = vec![entry(
            1,
            "2026-09-18T23:59:59-04:00",
            3600,
            "Practices - DevPro - Work",
            "Late",
        )];
        let result = run(&entries, &practices_config());
        assert_eq!(result[0].date, day(2026, 9, 18));
    }

    /// C2 / the plan's "timestamps arrive in two shapes". 36 % of the live window
    /// carries a numeric offset rather than `Z`, so a fixed-format parse taking
    /// only one shape would fail on more than a third of the data. Both forms
    /// describe the same instant here and must land on the same day.
    #[test]
    fn both_the_z_form_and_the_numeric_offset_form_parse_to_the_same_local_day() {
        let zulu = entry_local_date("2026-09-19T01:30:00Z", &edt()).expect("Z form parses");
        let offset =
            entry_local_date("2026-09-18T21:30:00-04:00", &edt()).expect("offset form parses");

        assert_eq!(zulu, day(2026, 9, 18));
        assert_eq!(offset, day(2026, 9, 18));
    }

    /// An unparseable timestamp is an error rather than a skip: `Instant.parse`
    /// throws out of the filter lambda and takes the whole aggregation with it.
    #[test]
    fn an_unparseable_start_time_fails_the_whole_aggregation() {
        let mut e = entry(
            1,
            "18/09/2026 13:00",
            3600,
            "Practices - DevPro - Work",
            "Docs",
        );
        e.start_time = "18/09/2026 13:00".to_string();
        let error = aggregate_in_zone(&[e], &practices_config(), None, None, &edt())
            .expect_err("a malformed timestamp must not be swallowed");
        assert!(
            error.to_string().contains("18/09/2026 13:00"),
            "the error names the offending value: {error}"
        );
    }

    /// C2 through the public entry point, which reads the system zone. Built from
    /// a *local* noon so the expectation holds in any zone the test machine sits
    /// in — the one assertion that proves `aggregate` actually wires `Local` in.
    #[test]
    fn the_public_entry_point_re_dates_through_the_system_zone() {
        let local_noon = Local
            .with_ymd_and_hms(2026, 9, 18, 12, 0, 0)
            .single()
            .expect("local noon is unambiguous");
        let entries = vec![entry(
            1,
            &local_noon.to_rfc3339(),
            3600,
            "Practices - DevPro - Work",
            "Docs",
        )];

        let result = aggregate(&entries, &practices_config(), None, None)
            .expect("aggregation should succeed");

        assert_eq!(result[0].date, day(2026, 9, 18));
    }

    // =======================================================================
    // Range bounds
    // =======================================================================

    /// `Aggregator.kt:50` — `date >= dateFrom`, inclusive.
    #[test]
    fn the_from_bound_is_inclusive() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "D",
        )];
        let result = aggregate_in_zone(
            &entries,
            &practices_config(),
            Some(day(2026, 9, 18)),
            None,
            &edt(),
        )
        .expect("aggregation should succeed");
        assert_eq!(result.len(), 1);
    }

    /// `Aggregator.kt:50` — `date <= dateTo`, inclusive.
    #[test]
    fn the_to_bound_is_inclusive() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "D",
        )];
        let result = aggregate_in_zone(
            &entries,
            &practices_config(),
            None,
            Some(day(2026, 9, 18)),
            &edt(),
        )
        .expect("aggregation should succeed");
        assert_eq!(result.len(), 1);
    }

    /// One day past each bound is excluded, which is what makes the two tests
    /// above assertions about inclusivity rather than about nothing.
    #[test]
    fn a_day_outside_either_bound_is_excluded() {
        let entries = vec![
            entry(
                1,
                "2026-09-17T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "D",
            ),
            entry(
                2,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "D",
            ),
            entry(
                3,
                "2026-09-19T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "D",
            ),
        ];
        let result = aggregate_in_zone(
            &entries,
            &practices_config(),
            Some(day(2026, 9, 18)),
            Some(day(2026, 9, 18)),
            &edt(),
        )
        .expect("aggregation should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].date, day(2026, 9, 18));
    }

    /// Both bounds absent means no date filtering at all — the default the
    /// Kotlin signature declares.
    #[test]
    fn absent_bounds_filter_nothing() {
        let entries = vec![
            entry(
                1,
                "2020-01-01T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "D",
            ),
            entry(
                2,
                "2030-12-31T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "D",
            ),
        ];
        assert_eq!(run(&entries, &practices_config()).len(), 2);
    }

    /// A `from` after `to` yields nothing rather than erroring — the two bounds
    /// are independent predicates, not a validated range.
    #[test]
    fn an_inverted_range_yields_nothing_rather_than_an_error() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "D",
        )];
        let result = aggregate_in_zone(
            &entries,
            &practices_config(),
            Some(day(2026, 9, 20)),
            Some(day(2026, 9, 10)),
            &edt(),
        )
        .expect("an inverted range is not an error");
        assert!(result.is_empty());
    }

    /// An entry filtered out by the range never reaches the mapping lookup, so an
    /// unmapped project outside the window cannot fail the run. The filter order
    /// at `Aggregator.kt:45-57` is what guarantees it.
    #[test]
    fn an_unmapped_project_outside_the_range_does_not_fail_the_run() {
        let entries = vec![
            entry(
                1,
                "2026-09-10T13:00:00Z",
                3600,
                "Velocitor - DevPro - Work",
                "NLP",
            ),
            entry(
                2,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Docs",
            ),
        ];
        let result = aggregate_in_zone(
            &entries,
            &practices_config(),
            Some(day(2026, 9, 18)),
            None,
            &edt(),
        )
        .expect("the unmapped entry is filtered before the mapping lookup");
        assert_eq!(result.len(), 1);
    }

    // =======================================================================
    // C9 — overrides before mappings, and the cap
    // =======================================================================

    /// C9. `Aggregator.kt:63-71`. The override wins over the `chrono_project`
    /// mapping, taking both the DevPro project and the billability with it.
    #[test]
    fn an_override_is_applied_before_the_project_mapping() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![OverrideRule {
                pattern: "Inveniam".to_string(),
                devpro_project: "Inveniam SOW #3".to_string(),
                billability: "Billable".to_string(),
                max_hours: None,
            }],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "Inveniam kickoff",
        )];
        let result = run(&entries, &config);

        assert_eq!(result[0].devpro_project_name, "Inveniam SOW #3");
        assert_eq!(result[0].billability, "Billable");
        assert_eq!(
            result[0].chrono_project, "Practices - DevPro - Work",
            "the Chrono project is recorded unchanged"
        );
    }

    /// C9, the decisive case: an override lets an entry through whose Chrono
    /// project has NO mapping at all. A port checking the mapping first would
    /// error here instead of aggregating, so this separates the two orders.
    #[test]
    fn an_override_resolves_a_chrono_project_that_has_no_mapping() {
        let config = config(
            Vec::new(),
            vec![override_rule("Inveniam", "Inveniam SOW #3", None)],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Velocitor - DevPro - Work",
            "Inveniam kickoff",
        )];

        let result = aggregate_in_zone(&entries, &config, None, None, &edt())
            .expect("an override is consulted before the mapping");
        assert_eq!(result[0].devpro_project_name, "Inveniam SOW #3");
    }

    /// C9. `max_hours` caps the aggregate's hours and is carried on the
    /// aggregate, which is what marks it non-scalable for the normalizer.
    #[test]
    fn a_matched_overrides_max_hours_caps_the_aggregate_and_is_recorded() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![override_rule("Ops", "Operations", Some(1.5))],
        );
        // Three hours of raw duration against a 1.5h cap.
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            10_800,
            "Practices - DevPro - Work",
            "Ops duty",
        )];
        let result = run(&entries, &config);

        assert_eq!(result[0].total_hours, 1.5);
        assert_eq!(result[0].max_hours, Some(1.5));
    }

    /// C9. The cap is `rawHours > maxHours`, strictly — under the cap the raw
    /// value survives untouched, and `max_hours` is still recorded.
    #[test]
    fn hours_under_the_cap_are_left_alone_and_still_carry_the_cap() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![override_rule("Ops", "Operations", Some(1.5))],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            1800,
            "Practices - DevPro - Work",
            "Ops duty",
        )];
        let result = run(&entries, &config);

        assert_eq!(result[0].total_hours, 0.5);
        assert_eq!(result[0].max_hours, Some(1.5));
    }

    /// C9. Exactly on the cap takes the `else` arm — `>` and not `>=`. The two
    /// arms produce the same number here, so the test asserts the cap is still
    /// recorded, which is the observable half.
    #[test]
    fn hours_landing_exactly_on_the_cap_are_unchanged() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![override_rule("Ops", "Operations", Some(1.0))],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "Ops duty",
        )];
        let result = run(&entries, &config);

        assert_eq!(result[0].total_hours, 1.0);
        assert_eq!(result[0].max_hours, Some(1.0));
    }

    /// C9. An override with no `max_hours` leaves the aggregate uncapped and
    /// scalable — `maxHours` stays null, which is the flag the normalizer reads.
    #[test]
    fn an_override_without_max_hours_leaves_the_aggregate_scalable() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![override_rule("Ops", "Operations", None)],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            10_800,
            "Practices - DevPro - Work",
            "Ops duty",
        )];
        let result = run(&entries, &config);

        assert_eq!(result[0].total_hours, 3.0);
        assert_eq!(result[0].max_hours, None);
    }

    /// C9. An aggregate resolved through the ordinary mapping never carries a
    /// cap: the mapping arm hands back `null` unconditionally.
    #[test]
    fn a_mapped_aggregate_never_carries_a_cap() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "D",
        )];
        assert_eq!(run(&entries, &practices_config())[0].max_hours, None);
    }

    /// C9. `overrides.find` — the FIRST matching rule wins, not the most
    /// specific and not the last.
    #[test]
    fn the_first_matching_override_wins_when_two_rules_match() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![
                override_rule("Ops", "First Match", Some(1.0)),
                override_rule("duty", "Second Match", Some(2.0)),
            ],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            10_800,
            "Practices - DevPro - Work",
            "Ops duty",
        )];
        let result = run(&entries, &config);

        assert_eq!(result[0].devpro_project_name, "First Match");
        assert_eq!(result[0].total_hours, 1.0);
    }

    /// C9. The pattern match is a case-insensitive substring, not a prefix and
    /// not an equality.
    #[test]
    fn override_patterns_match_case_insensitively_anywhere_in_the_description() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![override_rule("INVENIAM", "Inveniam SOW #3", None)],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "Weekly inveniam sync",
        )];
        assert_eq!(
            run(&entries, &config)[0].devpro_project_name,
            "Inveniam SOW #3"
        );
    }

    /// C9. A blank description never matches an override, so the entry falls
    /// through to its mapping — even against a rule that would otherwise match
    /// everything.
    #[test]
    fn a_blank_description_never_matches_an_override() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![override_rule("", "Never", Some(0.25))],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "   ",
        )];
        let result = run(&entries, &config);

        assert_eq!(result[0].devpro_project_name, "Delivery Practices");
        assert_eq!(result[0].max_hours, None);
    }

    /// C9, the counterpart: an empty pattern DOES match a non-blank description,
    /// because `indexOf("")` is 0. Together with the test above this pins that
    /// the guard is on the description, not on the pattern.
    #[test]
    fn an_empty_override_pattern_matches_every_non_blank_description() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![override_rule("", "Catch All", None)],
        );
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            3600,
            "Practices - DevPro - Work",
            "D",
        )];
        assert_eq!(run(&entries, &config)[0].devpro_project_name, "Catch All");
    }

    /// C9. The cap applies to the summed group, not to each entry — two entries
    /// sharing a description are capped once, after the sum.
    #[test]
    fn the_cap_applies_to_the_summed_group_not_to_each_entry() {
        let config = config(
            vec![mapping(
                "Practices - DevPro - Work",
                "Delivery Practices",
                "NonBillable",
            )],
            vec![override_rule("Ops", "Operations", Some(1.5))],
        );
        let entries = vec![
            entry(
                1,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Ops duty",
            ),
            entry(
                2,
                "2026-09-18T15:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Ops duty",
            ),
        ];
        let result = run(&entries, &config);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].total_hours, 1.5);
    }

    /// The per-character rule, on the case where the obvious port diverges.
    /// `"Xİ".to_lowercase()` is `x` + `i` + U+0307 — three characters where the
    /// original had two — so a whole-string fold reports a match for the
    /// two-character needle `i` + U+0307. Kotlin compares character by character
    /// against a two-character haystack and finds none.
    #[test]
    fn case_insensitive_matching_does_not_invent_characters_by_folding() {
        let haystack = "X\u{0130}";
        let needle = "i\u{0307}";

        assert!(
            haystack.to_lowercase().contains(needle),
            "the premise: a whole-string fold does match here"
        );
        assert!(
            !contains_ignore_case(haystack, needle),
            "a per-character comparison cannot match a 2-char needle against a 2-char \
             haystack whose first character is X"
        );
    }

    /// `Aggregator.kt:129` is `isBlank()`, not `isEmpty()`. Reached directly,
    /// because the only caller hands it an already-trimmed string and the two
    /// therefore coincide on every public path — the guard is ported as written
    /// and this is the assertion that says so.
    #[test]
    fn a_whitespace_only_description_matches_no_override_even_untrimmed() {
        let rules = vec![override_rule("", "Catch All", None)];
        assert!(find_override("   ", &rules).is_none());
        assert!(find_override("\t\n", &rules).is_none());
        assert!(find_override("", &rules).is_none());
        assert!(
            find_override("x", &rules).is_some(),
            "the premise: the rule does match"
        );
    }

    /// The two halves of the measured 82-code-point residue, one representative
    /// each, so the doc comment above cannot drift away from the behaviour.
    /// Both were read off a full code-point walk on JDK 21 versus rustc 1.91
    /// (`~/.cache/tt-devpro-rewrite/measurements/casecmp/`), not reasoned
    /// about.
    ///
    /// Neither is reachable on this tool's data — the live 2 710-entry window
    /// holds 43 distinct non-ASCII characters and none of the 82 — which is why
    /// they are locked as known divergences rather than repaired with a table.
    #[test]
    fn the_two_documented_case_folding_residues_behave_as_recorded() {
        // Half one: the JVM has a simple mapping, Rust exposes only the full one.
        // `Character.toUpperCase('\u1F80')` is U+1F88 on the JVM; the full Rust
        // mapping is three characters, so the fallback returns the input.
        assert_eq!(
            simple_uppercase('\u{1f80}'),
            '\u{1f80}',
            "Greek ypogegrammeni: the full mapping is multi-character, so it falls back"
        );
        assert!(
            '\u{1f80}'.to_uppercase().count() > 1,
            "the premise: Rust's full mapping really is multi-character here"
        );

        // U+0130, the case this comment originally named as the only one.
        assert_eq!(simple_lowercase('\u{130}'), '\u{130}');
        assert!('\u{130}'.to_lowercase().count() > 1);

        // Half two: rustc knows a case pair this JDK does not. Rust folds these
        // together and the JVM does not, so a needle that differs only by this
        // pair matches here and would not on the incumbent.
        assert!(
            contains_ignore_case("\u{a7cc}", "\u{a7cd}"),
            "Unicode version skew: rustc 1.91 pairs U+A7CC with U+A7CD, JDK 21 does not"
        );
    }

    /// `Character.toUpperCase(char)` returns the input unchanged when the full
    /// mapping would need more than one character, and `Character.toLowerCase`
    /// mirrors it. Rust's own `char::to_uppercase` is the full mapping, so
    /// substituting it would turn `ß` into `S` and start matching `strasse`
    /// against `straße`.
    #[test]
    fn the_case_helpers_are_the_single_character_java_mappings() {
        assert_eq!(simple_uppercase('ß'), 'ß', "the full mapping would be SS");
        assert_eq!(simple_uppercase('a'), 'A');
        assert_eq!(simple_uppercase('и'), 'И');
        assert_eq!(
            simple_lowercase('İ'),
            'İ',
            "the full mapping would be i + U+0307"
        );
        assert_eq!(simple_lowercase('A'), 'a');
        assert_eq!(simple_lowercase('И'), 'и');
        assert!(
            !contains_ignore_case("straße", "strasse"),
            "ß must not fold into ss"
        );
    }

    /// Cyrillic is 42 % of the live descriptions and folds one-to-one in both
    /// directions, so the ordinary case has to keep working.
    #[test]
    fn case_insensitive_matching_handles_cyrillic_in_both_directions() {
        assert!(contains_ignore_case("Разбор инцидента", "РАЗБОР"));
        assert!(contains_ignore_case("РАЗБОР ИНЦИДЕНТА", "инцидента"));
        assert!(!contains_ignore_case("Разбор инцидента", "релиз"));
    }

    /// The needle-longer-than-haystack guard — `indexOf` searches an empty range
    /// and returns -1 rather than reading past the end.
    #[test]
    fn a_pattern_longer_than_the_description_does_not_match() {
        assert!(!contains_ignore_case("sync", "weekly sync"));
    }

    /// The match must be anchored nowhere: start, middle and end all count, and a
    /// near-miss at the start must not stop the scan.
    #[test]
    fn a_pattern_matches_at_any_offset_including_after_a_false_start() {
        assert!(
            contains_ignore_case("aab", "ab"),
            "the scan continues past a partial match"
        );
        assert!(contains_ignore_case("sync weekly", "sync"));
        assert!(contains_ignore_case("weekly sync", "sync"));
        assert!(contains_ignore_case("sync", "sync"));
    }

    // =======================================================================
    // C28 — order
    // =======================================================================

    /// `Aggregator.kt:87`. The sort is (date, devproProjectName), so a later date
    /// sorts after an earlier one whatever the project names do.
    #[test]
    fn aggregates_sort_by_date_first_then_by_devpro_project_name() {
        let config = config(
            vec![
                mapping("Zebra - DevPro - Work", "Zebra Project", "Billable"),
                mapping("Alpha - DevPro - Work", "Alpha Project", "Billable"),
            ],
            Vec::new(),
        );
        let entries = vec![
            entry(
                1,
                "2026-09-18T13:00:00Z",
                3600,
                "Alpha - DevPro - Work",
                "A",
            ),
            entry(
                2,
                "2026-09-17T13:00:00Z",
                3600,
                "Zebra - DevPro - Work",
                "Z",
            ),
            entry(
                3,
                "2026-09-17T15:00:00Z",
                3600,
                "Alpha - DevPro - Work",
                "A",
            ),
        ];
        let result = run(&entries, &config);

        assert_eq!(
            result
                .iter()
                .map(|a| (a.date, a.devpro_project_name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (day(2026, 9, 17), "Alpha Project"),
                (day(2026, 9, 17), "Zebra Project"),
                (day(2026, 9, 18), "Alpha Project"),
            ]
        );
    }

    /// C28. Two aggregates tying on (date, devproProjectName) keep the order the
    /// grouping saw them in — Kotlin's `sortedWith` is a stable TimSort and the
    /// group map is a `LinkedHashMap`. `sort_unstable_by` or a `HashMap` here
    /// would reorder the rendered rows. The Chrono API's order is NOT
    /// chronological (measured on the captured day), so the entries below are
    /// deliberately out of time order to prove the encounter order is what wins.
    #[test]
    fn aggregates_tying_on_date_and_project_keep_their_encounter_order() {
        let entries = vec![
            entry(
                1,
                "2026-09-18T16:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Second hour",
            ),
            entry(
                2,
                "2026-09-18T13:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "First hour",
            ),
            entry(
                3,
                "2026-09-18T18:00:00Z",
                3600,
                "Practices - DevPro - Work",
                "Third hour",
            ),
        ];
        let result = run(&entries, &practices_config());

        assert_eq!(
            result
                .iter()
                .map(|a| a.descriptions[0].as_str())
                .collect::<Vec<_>>(),
            vec!["Second hour", "First hour", "Third hour"],
            "the Chrono array order survives grouping and the stable sort"
        );
    }

    /// C28, the stability claim with enough elements behind it to mean something.
    ///
    /// Rust's `sort_unstable_by` falls back to an insertion sort on short slices
    /// and detects an already-ordered run, so a handful of tied elements is
    /// preserved by both sorts and proves nothing — the plan says as much about
    /// the four-element measurement it made. Forty aggregates alternating between
    /// two dates is past both shortcuts: measured, the unstable sort reorders
    /// this arrangement and the stable one does not.
    #[test]
    fn a_long_run_of_ties_keeps_its_encounter_order_under_the_stable_sort() {
        let entries: Vec<ChronoTimeEntry> = (0..40)
            .map(|i| {
                let start = if i % 2 == 0 {
                    "2026-09-17T13:00:00Z"
                } else {
                    "2026-09-18T13:00:00Z"
                };
                entry(
                    i,
                    start,
                    3600,
                    "Practices - DevPro - Work",
                    &format!("Task {i}"),
                )
            })
            .collect();

        let result = run(&entries, &practices_config());

        let expected: Vec<String> = (0..40)
            .filter(|i| i % 2 == 0)
            .chain((0..40).filter(|i| i % 2 == 1))
            .map(|i| format!("Task {i}"))
            .collect();

        assert_eq!(
            result
                .iter()
                .map(|a| a.descriptions[0].clone())
                .collect::<Vec<_>>(),
            expected,
            "every tie must keep the order the grouping saw it in"
        );
    }

    /// C28 at the call site rather than at the helper: two DevPro project names
    /// whose UTF-16 and UTF-8 orders disagree, run through `aggregate` itself.
    /// U+1D400 is an astral character, so UTF-16 sorts it by its lead surrogate
    /// (0xD835) and puts it before U+FB00, while its UTF-8 bytes put it after.
    /// The direct `utf16_cmp` test below cannot catch a comparator swapped inside
    /// the sort; this one can.
    #[test]
    fn the_sort_compares_project_names_as_utf16_not_as_utf8_bytes() {
        let config = config(
            vec![
                mapping("Astral - DevPro - Work", "\u{1D400}", "Billable"),
                mapping("Bmp - DevPro - Work", "\u{FB00}", "Billable"),
            ],
            Vec::new(),
        );
        let entries = vec![
            entry(1, "2026-09-18T13:00:00Z", 3600, "Bmp - DevPro - Work", "B"),
            entry(
                2,
                "2026-09-18T15:00:00Z",
                3600,
                "Astral - DevPro - Work",
                "A",
            ),
        ];
        let result = run(&entries, &config);

        assert_eq!(
            result
                .iter()
                .map(|a| a.devpro_project_name.as_str())
                .collect::<Vec<_>>(),
            vec!["\u{1D400}", "\u{FB00}"],
            "UTF-16 order puts the astral name first; UTF-8 byte order would not"
        );
    }

    /// C28's cheap guard: the same input aggregated twice gives byte-identical
    /// output. Fails instantly if a randomized `HashMap` ever reaches the
    /// grouping or the ordering.
    #[test]
    fn aggregating_the_same_input_twice_gives_the_same_order() {
        let entries: Vec<ChronoTimeEntry> = (0..24)
            .map(|i| {
                entry(
                    i,
                    "2026-09-18T13:00:00Z",
                    3600,
                    "Practices - DevPro - Work",
                    &format!("Task {i}"),
                )
            })
            .collect();

        let first = run(&entries, &practices_config());
        let second = run(&entries, &practices_config());

        assert_eq!(first, second);
        assert_eq!(
            first
                .iter()
                .map(|a| a.descriptions[0].as_str())
                .collect::<Vec<_>>(),
            (0..24).map(|i| format!("Task {i}")).collect::<Vec<_>>()
        );
    }

    /// `String.compareTo` compares UTF-16 code units, not UTF-8 bytes. The two
    /// agree throughout the Basic Multilingual Plane; this pins the comparator so
    /// a swap to `str::cmp` is a deliberate act rather than an accident.
    #[test]
    fn project_names_sort_by_utf16_code_units() {
        assert_eq!(utf16_cmp("Artory", "Presales"), Ordering::Less);
        assert_eq!(utf16_cmp("Presales", "Presales"), Ordering::Equal);
        assert_eq!(
            utf16_cmp("Presales", "Pre"),
            Ordering::Greater,
            "a prefix sorts before the longer string"
        );
        // U+1D400 (astral) vs U+FB00 (BMP): UTF-16 puts the surrogate pair
        // (0xD835) below 0xFB00, while UTF-8 bytes put the astral one above.
        assert_eq!(utf16_cmp("\u{1D400}", "\u{FB00}"), Ordering::Less);
    }

    // =======================================================================
    // Arithmetic
    // =======================================================================

    /// `Aggregator.kt:73-74`. Seconds sum as integers and divide by 3600.0 once,
    /// so no per-entry rounding creeps in. 450 s is exactly 0.125 h — one of the
    /// values the `%.Nf` work found diverging, which is why it is the sample.
    #[test]
    fn hours_are_the_summed_seconds_divided_once() {
        let entries = vec![
            entry(
                1,
                "2026-09-18T13:00:00Z",
                450,
                "Practices - DevPro - Work",
                "D",
            ),
            entry(
                2,
                "2026-09-18T15:00:00Z",
                450,
                "Practices - DevPro - Work",
                "D",
            ),
        ];
        let result = run(&entries, &practices_config());
        assert_eq!(result[0].total_hours, 0.25);
    }

    /// A single odd-second duration is carried as the exact quotient, not
    /// quantized — quantization happens downstream in the normalizer.
    #[test]
    fn an_odd_duration_is_not_quantized_by_the_aggregator() {
        let entries = vec![entry(
            1,
            "2026-09-18T13:00:00Z",
            1350,
            "Practices - DevPro - Work",
            "D",
        )];
        assert_eq!(run(&entries, &practices_config())[0].total_hours, 0.375);
    }

    /// An empty entry list aggregates to an empty result rather than erroring.
    #[test]
    fn no_entries_aggregate_to_nothing() {
        assert!(run(&[], &practices_config()).is_empty());
    }
}
