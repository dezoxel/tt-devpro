//! `~/.config/tt-devpro/config.yaml` — the structs and the loader.
//!
//! Ports `config/Config.kt` and `config/ConfigLoader.kt`.
//!
//! Every struct here carries `deny_unknown_fields` (C30). `kaml`'s `Yaml.default`
//! has `strictMode = true`, so the incumbent rejects an unknown key outright, and
//! serde is lenient unless told otherwise. The strictness is load-bearing rather
//! than pedantic: a typo'd key under `mappings` would, under a lenient parser,
//! become a mapping that silently does not exist, and an unmapped Chrono project
//! is silently skipped (C10) — so the failure would surface as missing hours on a
//! real day with nothing pointing at the config.

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub chrono_api: String,
    pub mappings: Vec<ProjectMapping>,
    #[serde(default)]
    pub fillers: Vec<Filler>,
    #[serde(default)]
    pub overrides: Vec<OverrideRule>,
    #[serde(default)]
    pub project_ids: HashMap<String, String>,
    /// G4: this key appears nowhere in the live config, so the default is what
    /// every real run uses.
    #[serde(default = "default_max_synthetic_hours")]
    pub max_synthetic_hours: f64,
    /// The model `settle` plans with, passed to `claude -p --model` as written.
    #[serde(default = "default_plan_model")]
    pub plan_model: String,
    /// The Obsidian vault whose `Calendar` folders mark meetings. Required, with no
    /// default: the vault sits at a different path on each machine, and the
    /// incumbent's hard-coded `~/knowledge-base` silently found no meetings wherever
    /// it was elsewhere. Absolute; `~` is not expanded. Named after ergon's
    /// `vault_path`, which holds the same path.
    pub vault_path: PathBuf,
    /// 1Password secret reference to the portal session cookie, e.g.
    /// `op://Dev.Pro/TT DevPro Session/credential`. A reference, not the secret:
    /// `cookie::session_cookie` reads the value through `op read` on every run,
    /// and `make auth` writes it there. Optional only because `TT_COOKIE` can
    /// stand in for it.
    #[serde(default)]
    pub session_cookie: Option<String>,
    /// The planned billable allocation per DevPro project, in FTE. The portal frontend
    /// shows FTE only to an approver (`teamFte`), not to the person logging time, so the
    /// period line in `settle` reads it from here. An entry holds `[from, to]`, and a
    /// project may have several, so a mid-period change is counted day by day.
    /// Optional: without it the line shows the FTE so far and no pace.
    #[serde(default)]
    pub allocations: Vec<Allocation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMapping {
    pub chrono_project: String,
    pub devpro_project: String,
    pub billability: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filler {
    pub devpro_project: String,
    pub task_title: String,
    pub billability: String,
    pub min_hours: f64,
    pub max_hours: f64,
}

/// `fte` of 0.5 is half of a full-time week on that project, on every day from `from` to
/// `to`, both included, as the allocation email states them. Both dates are required: an
/// entry with no dates was planned at the new FTE for the whole period it changed in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub devpro_project: String,
    pub fte: f64,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl Allocation {
    /// Whether the entry is in force on `day`.
    pub fn holds(&self, day: NaiveDate) -> bool {
        self.from <= day && day <= self.to
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverrideRule {
    pub pattern: String,
    pub devpro_project: String,
    pub billability: String,
    #[serde(default)]
    pub max_hours: Option<f64>,
}

fn default_max_synthetic_hours() -> f64 {
    4.0
}

/// Sonnet, not Haiku: on the replay of 23.09-06.10 Haiku broke the planner's rules on the
/// same prompt (communication as a day's main work) and its titles drifted from run to run.
fn default_plan_model() -> String {
    "sonnet".to_string()
}

/// The body of `ConfigLoader.kt:10-21`, after `trimIndent()`.
const MISSING_CONFIG_HELP: &str = r#"

Create ~/.config/tt-devpro/config.yaml with:

chrono_api: "http://localhost:9247"
vault_path: "/absolute/path/to/your/vault"
session_cookie: "op://Vault/Item/field"

mappings:
  - chrono_project: "Your Chrono Project"
    devpro_project: "DevPro Project Name"
    billability: "Billable""#;

/// `ConfigLoader.kt:6`, moved. The incumbent read `~/.tt-config.yaml`; the file now
/// sits beside the other tools' configs, at `~/.config/tt-devpro/config.yaml`.
/// Every message that names the file names the new place, so the one parity
/// baseline that prints the path (`settle-range-aug-json.err`) differs in that line.
///
/// Built from `$HOME` alone, not from `$XDG_CONFIG_HOME`: the path is then the same
/// on every OS, and the tests keep one variable to point at an empty directory.
/// `dirs::home_dir()` rather than the JVM's `System.getProperty("user.home")`: the
/// two agree on every ordinary run and differ only under an overridden `$HOME`,
/// where the Rust one is the testable behaviour and the JVM one is the reason an
/// attempt to point the incumbent at a temporary config directory had no effect at
/// all (C30).
pub fn config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not determine the home directory"))?;
    Ok(home.join(".config").join("tt-devpro").join("config.yaml"))
}

pub fn load() -> Result<Config> {
    load_from(&config_path()?)
}

pub fn load_from(path: &Path) -> Result<Config> {
    if !path.exists() {
        bail!(
            "Config file not found: {}{}",
            path.display(),
            MISSING_CONFIG_HELP
        );
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config: {}", path.display()))?;
    parse(&content)
}

pub fn parse(content: &str) -> Result<Config> {
    let config: Config =
        serde_yaml::from_str(content).map_err(|e| anyhow!("Failed to parse config: {e}"))?;
    if !config.vault_path.is_absolute() {
        bail!(
            "vault_path in ~/.config/tt-devpro/config.yaml must be an absolute path (`~` is not \
             expanded): {}",
            config.vault_path.display()
        );
    }
    if let Some(reference) = &config.session_cookie {
        if !reference.starts_with("op://") {
            bail!(
                "session_cookie in ~/.config/tt-devpro/config.yaml must be a 1Password reference \
                 (op://vault/item/field), not the cookie itself"
            );
        }
    }
    for (index, allocation) in config.allocations.iter().enumerate() {
        if !(allocation.fte > 0.0 && allocation.fte <= 1.0) {
            bail!(
                "allocations in ~/.config/tt-devpro/config.yaml: fte of {} must be above 0 and at \
                 most 1, got {}",
                allocation.devpro_project,
                allocation.fte
            );
        }
        if allocation.from > allocation.to {
            bail!(
                "allocations in ~/.config/tt-devpro/config.yaml: {} runs from {} to {}, which \
                 ends before it starts",
                allocation.devpro_project,
                allocation.from,
                allocation.to
            );
        }
        // Pairwise against every entry above, so the order they are listed in does not matter.
        if let Some(other) = config.allocations[..index].iter().find(|other| {
            other.devpro_project == allocation.devpro_project
                && other.from <= allocation.to
                && allocation.from <= other.to
        }) {
            bail!(
                "allocations in ~/.config/tt-devpro/config.yaml: {} has two entries in force on \
                 the same days, {}..{} and {}..{}",
                allocation.devpro_project,
                other.from,
                other.to,
                allocation.from,
                allocation.to
            );
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
chrono_api: "http://localhost:9247"
vault_path: "/vault"
mappings:
  - chrono_project: "Practices - DevPro - Work"
    devpro_project: "Delivery Practices"
    billability: "NonBillable"
"#;

    #[test]
    fn parses_a_minimal_config_and_defaults_the_rest() {
        let config = parse(MINIMAL).expect("minimal config should parse");
        assert_eq!(config.chrono_api, "http://localhost:9247");
        assert_eq!(config.mappings.len(), 1);
        assert!(config.fillers.is_empty());
        assert!(config.overrides.is_empty());
        assert!(config.project_ids.is_empty());
        assert!(config.allocations.is_empty());
        // G4: no key in the live config, so this is the value every real run uses.
        assert_eq!(config.max_synthetic_hours, 4.0);
    }

    /// C30. `kaml` runs with `strictMode = true`, so an unknown key is an error
    /// rather than something to ignore.
    #[test]
    fn rejects_an_unknown_top_level_key() {
        let yaml = format!("{MINIMAL}unknown_key: 1\n");
        let err = parse(&yaml).expect_err("an unknown key must not parse");
        assert!(
            err.to_string().starts_with("Failed to parse config: "),
            "unexpected message: {err}"
        );
    }

    /// The one that actually costs hours if it is dropped: a typo'd key under
    /// `mappings` becomes a mapping that does not exist, and an unmapped Chrono
    /// project is skipped without a word (C10).
    #[test]
    fn rejects_an_unknown_key_inside_a_mapping() {
        let yaml = r#"
chrono_api: "http://localhost:9247"
vault_path: "/vault"
mappings:
  - chrono_project: "Practices - DevPro - Work"
    devpro_projekt: "Delivery Practices"
    billability: "NonBillable"
"#;
        assert!(parse(yaml).is_err(), "a typo'd mapping key must not parse");
    }

    #[test]
    fn rejects_an_unknown_key_inside_a_filler_or_an_override() {
        let filler = r#"
chrono_api: "x"
vault_path: "/vault"
mappings: []
fillers:
  - devpro_project: "P"
    task_title: "T"
    billability: "Billable"
    min_hours: 0.5
    max_hours: 2.0
    nonsense: true
"#;
        assert!(parse(filler).is_err());

        let override_rule = r#"
chrono_api: "x"
vault_path: "/vault"
mappings: []
overrides:
  - pattern: "x"
    devpro_project: "P"
    billability: "Billable"
    nonsense: true
"#;
        assert!(parse(override_rule).is_err());
    }

    /// The per-period filler budget went with the old filler service. Accepted and ignored, it
    /// would read as a cap that nothing enforces, so it is refused like any unknown key.
    #[test]
    fn a_per_period_filler_budget_is_refused_now_that_nothing_enforces_it() {
        let yaml = r#"
chrono_api: "http://localhost:9247"
vault_path: "/vault"
mappings: []
fillers:
  - devpro_project: "P"
    task_title: "T"
    billability: "NonBillable"
    min_hours: 0.5
    max_hours: 2.0
    max_hours_per_period: 10.0
"#;
        assert!(parse(yaml).is_err());
    }

    #[test]
    fn reads_every_optional_section_when_present() {
        let yaml = r#"
chrono_api: "http://localhost:9247"
vault_path: "/vault"
mappings:
  - chrono_project: "A"
    devpro_project: "B"
    billability: "Billable"
fillers:
  - devpro_project: "P"
    task_title: "T"
    billability: "NonBillable"
    min_hours: 0.5
    max_hours: 2.0
overrides:
  - pattern: "meeting"
    devpro_project: "P"
    billability: "NonBillable"
    max_hours: 1.5
project_ids:
  "Delivery Practices": "cf84fdca-4809-4678-98b1-2e7cc56537c0"
max_synthetic_hours: 2.5
"#;
        let config = parse(yaml).expect("full config should parse");
        assert_eq!(config.fillers[0].max_hours, 2.0);
        assert_eq!(config.overrides[0].max_hours, Some(1.5));
        assert_eq!(
            config
                .project_ids
                .get("Delivery Practices")
                .map(String::as_str),
            Some("cf84fdca-4809-4678-98b1-2e7cc56537c0")
        );
        assert_eq!(config.max_synthetic_hours, 2.5);
    }

    /// No default: a config without the vault path must not parse, or `settle`
    /// would go back to guessing where the vault is.
    #[test]
    fn vault_path_is_required() {
        let yaml = MINIMAL.replace("vault_path: \"/vault\"\n", "");
        assert!(
            parse(&yaml).is_err(),
            "a config without vault_path must not parse"
        );
    }

    #[test]
    fn vault_path_must_be_absolute() {
        for relative in ["digital-brain", "~/digital-brain"] {
            let yaml = MINIMAL.replace("/vault", relative);
            let message = parse(&yaml)
                .expect_err("a relative path must not parse")
                .to_string();
            assert!(message.contains("must be an absolute path"), "{message}");
        }
    }

    #[test]
    fn session_cookie_is_optional_and_read_as_a_reference() {
        assert_eq!(parse(MINIMAL).expect("parses").session_cookie, None);
        let yaml =
            format!("{MINIMAL}session_cookie: \"op://Dev.Pro/TT DevPro Session/credential\"\n");
        assert_eq!(
            parse(&yaml).expect("parses").session_cookie.as_deref(),
            Some("op://Dev.Pro/TT DevPro Session/credential")
        );
    }

    const ALLOCATION: &str = "  - devpro_project: \"Inveniam SOW #5\"\n";

    /// One entry: the project, its fte and the days it holds.
    fn entry(project: &str, fte: &str, from: &str, to: &str) -> String {
        format!(
            "  - devpro_project: \"{project}\"\n    fte: {fte}\n    from: {from}\n    to: {to}\n"
        )
    }

    fn allocations(entries: &[String]) -> String {
        format!("{MINIMAL}allocations:\n{}", entries.concat())
    }

    fn day(month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, month, day).unwrap()
    }

    /// The allocation email of 2026-10-12, as it is written in the config.
    #[test]
    fn allocations_read_a_project_its_fte_and_its_days() {
        let yaml = allocations(&[
            entry("Inveniam SOW #5", "0.5", "2026-08-28", "2026-10-11"),
            entry("Inveniam SOW #5", "0.75", "2026-10-12", "2026-12-12"),
        ]);
        assert_eq!(
            parse(&yaml).expect("parses").allocations,
            vec![
                Allocation {
                    devpro_project: "Inveniam SOW #5".to_string(),
                    fte: 0.5,
                    from: day(8, 28),
                    to: day(10, 11),
                },
                Allocation {
                    devpro_project: "Inveniam SOW #5".to_string(),
                    fte: 0.75,
                    from: day(10, 12),
                    to: day(12, 12),
                },
            ]
        );
    }

    #[test]
    fn an_allocation_holds_both_of_its_dates() {
        let allocation = Allocation {
            devpro_project: "Inveniam SOW #5".to_string(),
            fte: 0.5,
            from: day(10, 12),
            to: day(10, 15),
        };
        for (date, holds) in [(11, false), (12, true), (15, true), (16, false)] {
            assert_eq!(allocation.holds(day(10, date)), holds, "{date}");
        }
    }

    /// An fte of 0 would divide by zero in the pace; above 1 is a typo for a percentage.
    #[test]
    fn an_allocation_fte_outside_zero_to_one_is_refused() {
        for fte in ["0", "-0.5", "50", ".nan"] {
            let yaml = allocations(&[entry("Inveniam SOW #5", fte, "2026-10-01", "2026-10-15")]);
            let message = parse(&yaml).expect_err(fte).to_string();
            assert!(
                message.contains("must be above 0 and at most 1"),
                "{message}"
            );
        }
    }

    /// An entry with no dates is what planned all of 1–15 October at the new fte.
    #[test]
    fn an_allocation_without_its_dates_is_refused() {
        for dates in ["", "    from: 2026-10-01\n", "    to: 2026-10-15\n"] {
            let yaml = format!("{MINIMAL}allocations:\n{ALLOCATION}    fte: 0.5\n{dates}");
            assert!(parse(&yaml).is_err(), "{dates:?}");
        }
    }

    #[test]
    fn an_allocation_ending_before_it_starts_is_refused() {
        let yaml = allocations(&[entry("Inveniam SOW #5", "0.5", "2026-10-15", "2026-10-01")]);
        let message = parse(&yaml).expect_err("reversed").to_string();
        assert!(
            message.contains("runs from 2026-10-15 to 2026-10-01, which ends before it starts"),
            "{message}"
        );
    }

    /// Two entries in force on one day would leave the reader to guess which one counts. The
    /// later entry listed first is the same overlap.
    #[test]
    fn two_entries_of_one_project_on_the_same_days_are_refused() {
        let early = entry("Inveniam SOW #5", "0.5", "2026-08-28", "2026-10-12");
        let late = entry("Inveniam SOW #5", "0.75", "2026-10-12", "2026-12-12");
        for entries in [[early.clone(), late.clone()], [late, early]] {
            let message = parse(&allocations(&entries))
                .expect_err("overlap")
                .to_string();
            assert!(
                message.contains("Inveniam SOW #5 has two entries in force on the same days"),
                "{message}"
            );
        }
        let same = entry("Inveniam SOW #5", "0.5", "2026-10-01", "2026-10-15");
        assert!(parse(&allocations(&[same.clone(), same])).is_err());
    }

    #[test]
    fn adjacent_entries_and_other_projects_on_the_same_days_are_accepted() {
        let yaml = allocations(&[
            entry("Inveniam SOW #5", "0.75", "2026-10-12", "2026-12-12"),
            entry("Inveniam SOW #5", "0.5", "2026-08-28", "2026-10-11"),
            entry("Delivery Practices", "0.25", "2026-10-12", "2026-12-12"),
        ]);
        assert_eq!(parse(&yaml).expect("parses").allocations.len(), 3);
    }

    #[test]
    fn an_unknown_key_inside_an_allocation_is_refused() {
        let mut yaml = allocations(&[entry("Inveniam SOW #5", "0.5", "2026-10-01", "2026-10-15")]);
        yaml.push_str("    hours: 4\n");
        assert!(parse(&yaml).is_err());
    }

    /// The cookie itself must never sit in the config file.
    #[test]
    fn session_cookie_rejects_a_raw_cookie() {
        let yaml = format!("{MINIMAL}session_cookie: \"SESSION=abc123\"\n");
        let message = parse(&yaml)
            .expect_err("a raw cookie must not parse")
            .to_string();
        assert!(
            message.contains("must be a 1Password reference"),
            "{message}"
        );
    }

    #[test]
    fn a_missing_file_names_the_path_and_shows_the_template() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let err = load_from(&path).expect_err("a missing config must be an error");
        let message = err.to_string();
        assert!(message.starts_with(&format!("Config file not found: {}", path.display())));
        assert!(message.contains("chrono_api: \"http://localhost:9247\""));
        assert!(message.contains("billability: \"Billable\""));
    }
}
