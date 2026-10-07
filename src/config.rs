//! `~/.tt-config.yaml` — the structs and the loader.
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
    #[serde(default)]
    pub max_hours_per_period: Option<f64>,
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

/// The body of `ConfigLoader.kt:10-21`, after `trimIndent()`.
const MISSING_CONFIG_HELP: &str = r#"

Create ~/.tt-config.yaml with:

chrono_api: "http://localhost:9247"
vault_path: "/absolute/path/to/your/vault"
session_cookie: "op://Vault/Item/field"

mappings:
  - chrono_project: "Your Chrono Project"
    devpro_project: "DevPro Project Name"
    billability: "Billable""#;

/// `ConfigLoader.kt:6`. `dirs::home_dir()` rather than the JVM's
/// `System.getProperty("user.home")`: the two agree on every ordinary run and
/// differ only under an overridden `$HOME`, where the Rust one is the testable
/// behaviour and the JVM one is the reason an attempt to point the incumbent at a
/// temporary config directory had no effect at all (C30).
pub fn config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not determine the home directory"))?;
    Ok(home.join(".tt-config.yaml"))
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
            "vault_path in ~/.tt-config.yaml must be an absolute path (`~` is not \
             expanded): {}",
            config.vault_path.display()
        );
    }
    if let Some(reference) = &config.session_cookie {
        if !reference.starts_with("op://") {
            bail!(
                "session_cookie in ~/.tt-config.yaml must be a 1Password reference \
                 (op://vault/item/field), not the cookie itself"
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
    max_hours_per_period: 10.0
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
        assert_eq!(config.fillers[0].max_hours_per_period, Some(10.0));
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
        let path = dir.path().join(".tt-config.yaml");
        let err = load_from(&path).expect_err("a missing config must be an error");
        let message = err.to_string();
        assert!(message.starts_with(&format!("Config file not found: {}", path.display())));
        assert!(message.contains("chrono_api: \"http://localhost:9247\""));
        assert!(message.contains("billability: \"Billable\""));
    }
}
