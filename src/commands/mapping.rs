//! `tt-devpro mapping add` — one Chrono project mapped to a DevPro project in
//! `~/.config/tt-devpro/config.yaml`.
//!
//! A day with an unmapped Chrono project is left out of the plan (`settle` reports it as a day
//! error), and this command is the fix the error points at. It edits the file as text rather
//! than re-serialising the YAML, because a round trip through serde would drop every comment in
//! the config. The edited text is loaded back through the ordinary loader before it replaces the
//! file, so an insertion that went wrong is refused with the file untouched.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDate};
use clap::Args;

use super::{Console, Outcome};
use crate::api::portal::TtApiClient;
use crate::config::{self, ProjectMapping};

/// `mapping --help`, and what `mapping` with no subcommand prints.
pub const MAPPING_HELP: &str = r#"Usage: tt-devpro mapping [<options>] <command> [<args>]...

  Manage Chrono → DevPro project mappings in the config

Options:
  -h, --help  Show this message and exit

Commands:
  add  Map a Chrono project to a DevPro project"#;

/// `mapping add --help`.
pub const MAPPING_ADD_HELP: &str = r#"Usage: tt-devpro mapping add [<options>]

  Map a Chrono project to a DevPro project

Options:
  --chrono-project=<text>  Chrono project name, as Chrono shows it
  --devpro-project=<text>  DevPro project name, assigned to you today
  --billability=<text>     Billable or NonBillable
  -h, --help               Show this message and exit"#;

const BILLABILITIES: [&str; 2] = ["Billable", "NonBillable"];

#[derive(Args, Debug, Clone)]
pub struct MappingAddArgs {
    #[arg(long = "chrono-project")]
    pub chrono_project: String,
    #[arg(long = "devpro-project")]
    pub devpro_project: String,
    #[arg(long = "billability")]
    pub billability: String,
}

pub async fn run_add(args: &MappingAddArgs, io: &mut dyn Console) -> Outcome {
    let today = Local::now().date_naive();
    let result = match config::config_path() {
        Ok(path) => match crate::cookie::session_cookie().and_then(TtApiClient::new) {
            Ok(portal) => add(&portal, &path, args, today, io).await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => Outcome::Ok,
        Err(error) => {
            io.err(&format!("\u{2717} Error: {error:#}"));
            Outcome::Failed
        }
    }
}

/// Every check that needs no network runs before the portal is asked, so a typo in the
/// billability or a duplicate costs no request.
async fn add(
    portal: &TtApiClient,
    path: &Path,
    args: &MappingAddArgs,
    today: NaiveDate,
    io: &mut dyn Console,
) -> Result<()> {
    if !BILLABILITIES.contains(&args.billability.as_str()) {
        bail!(
            "--billability must be Billable or NonBillable, not `{}`",
            args.billability
        );
    }
    let current = config::load_from(path)?;
    if let Some(existing) = current
        .mappings
        .iter()
        .find(|mapping| mapping.chrono_project == args.chrono_project)
    {
        bail!(
            "{} is already mapped to {} as {} in {}",
            existing.chrono_project,
            existing.devpro_project,
            existing.billability,
            path.display()
        );
    }

    let user = portal.get_current_user().await?;
    let assigned = portal
        .get_assigned_projects(&user.unique_id, &today.to_string())
        .await?;
    if !assigned
        .projects
        .iter()
        .any(|project| project.short_name == args.devpro_project)
    {
        let names: Vec<&str> = assigned
            .projects
            .iter()
            .map(|project| project.short_name.as_str())
            .collect();
        bail!(
            "{} is not assigned to you on {today}. Assigned: {}",
            args.devpro_project,
            names.join(", ")
        );
    }

    let mapping = ProjectMapping {
        chrono_project: args.chrono_project.clone(),
        devpro_project: args.devpro_project.clone(),
        billability: args.billability.clone(),
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config: {}", path.display()))?;
    let edited = insert_mapping(&text, &mapping)?;
    replace_checked(path, &edited, &current.mappings, &mapping)?;

    io.out(&format!(
        "\u{2713} Mapped {} → {} ({}) in {}",
        mapping.chrono_project,
        mapping.devpro_project,
        mapping.billability,
        path.display()
    ));
    Ok(())
}

/// Writes `edited` beside the config, loads it back and renames it over the config only if
/// the mappings came out as the old ones plus `added`, in that order. Anything else removes
/// the temporary file and leaves the config as it was.
fn replace_checked(
    path: &Path,
    edited: &str,
    before: &[ProjectMapping],
    added: &ProjectMapping,
) -> Result<()> {
    let temp = path.with_extension("yaml.tmp");
    std::fs::write(&temp, edited).with_context(|| format!("Failed to write {}", temp.display()))?;
    let check = config::load_from(&temp).and_then(|loaded| {
        let mut expected = before.to_vec();
        expected.push(added.clone());
        if loaded.mappings == expected {
            Ok(())
        } else {
            bail!("the edited config does not read back as the old mappings plus the new one")
        }
    });
    if let Err(error) = check {
        let _ = std::fs::remove_file(&temp);
        return Err(error.context(format!("{} was not changed", path.display())));
    }
    std::fs::rename(&temp, path).with_context(|| format!("Failed to replace {}", path.display()))
}

/// The config text with `mapping` appended to the `mappings:` block.
///
/// The block starts at `mappings:` in column 0 and runs to the next line in column 0 that is
/// neither blank nor a comment. The new item goes after the block's last line that is neither
/// blank nor a comment, so a comment introducing the next block (the live config has
/// `# Fillers` there) stays in front of that block. The item takes the indent of the block's
/// first `- ` line.
fn insert_mapping(text: &str, mapping: &ProjectMapping) -> Result<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let Some(header) = lines.iter().position(|line| is_mappings_header(line)) else {
        bail!("the config has no `mappings:` block in column 0");
    };
    let block_end = lines[header + 1..]
        .iter()
        .position(|line| starts_next_block(line))
        .map_or(lines.len(), |offset| header + 1 + offset);
    let block = &lines[header + 1..block_end];
    let Some(last) = block.iter().rposition(|line| is_content(line)) else {
        bail!("the `mappings:` block has no items to add after");
    };
    let Some(indent) = block
        .iter()
        .find(|line| line.trim_start().starts_with("- "))
        .map(|line| &line[..line.len() - line.trim_start().len()])
    else {
        bail!("the `mappings:` block is not a list of `- ` items");
    };

    let item = [
        format!(
            "{indent}- chrono_project: {}",
            quoted(&mapping.chrono_project)
        ),
        format!(
            "{indent}  devpro_project: {}",
            quoted(&mapping.devpro_project)
        ),
        format!("{indent}  billability: {}", quoted(&mapping.billability)),
    ];
    let at = header + 1 + last + 1;
    let mut out: Vec<String> = lines[..at].iter().map(|line| line.to_string()).collect();
    out.extend(item);
    out.extend(lines[at..].iter().map(|line| line.to_string()));
    Ok(out.join("\n"))
}

/// `mappings:` with nothing after it but an optional comment. An inline value
/// (`mappings: []`) is a flow list this text edit does not handle, so it is not a header.
fn is_mappings_header(line: &str) -> bool {
    line.strip_prefix("mappings:").is_some_and(|rest| {
        let rest = rest.trim();
        rest.is_empty() || rest.starts_with('#')
    })
}

fn starts_next_block(line: &str) -> bool {
    !line.is_empty() && !line.starts_with([' ', '\t', '#'])
}

fn is_content(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !trimmed.starts_with('#')
}

/// A YAML double-quoted scalar. Chrono and DevPro names are plain text, so escaping the two
/// characters that would end or break the quotes is all it takes.
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::api::stub::{StubServer, json_200};

    /// The live config's layout: a comment inside the block, a comment introducing the next
    /// block, and the next block itself.
    const CONFIG: &str = r#"chrono_api: "http://localhost:9247"
vault_path: "/vault"

mappings:
  # Client work
  - chrono_project: "Inveniam - DevPro - Work"
    devpro_project: "Inveniam SOW #5"
    billability: "Billable"
  - chrono_project: "AI Practices - DevPro - Work"
    devpro_project: "AI Practices"
    billability: "NonBillable"

# Fillers
fillers:
  - devpro_project: "AI Practices"
    task_title: "AI research"
    billability: "NonBillable"
    min_hours: 0.5
    max_hours: 1.5
"#;

    const EDITED: &str = r#"chrono_api: "http://localhost:9247"
vault_path: "/vault"

mappings:
  # Client work
  - chrono_project: "Inveniam - DevPro - Work"
    devpro_project: "Inveniam SOW #5"
    billability: "Billable"
  - chrono_project: "AI Practices - DevPro - Work"
    devpro_project: "AI Practices"
    billability: "NonBillable"
  - chrono_project: "Presales - DevPro - Work"
    devpro_project: "Presales"
    billability: "NonBillable"

# Fillers
fillers:
  - devpro_project: "AI Practices"
    task_title: "AI research"
    billability: "NonBillable"
    min_hours: 0.5
    max_hours: 1.5
"#;

    fn presales() -> ProjectMapping {
        ProjectMapping {
            chrono_project: "Presales - DevPro - Work".to_string(),
            devpro_project: "Presales".to_string(),
            billability: "NonBillable".to_string(),
        }
    }

    fn args(chrono: &str, devpro: &str, billability: &str) -> MappingAddArgs {
        MappingAddArgs {
            chrono_project: chrono.to_string(),
            devpro_project: devpro.to_string(),
            billability: billability.to_string(),
        }
    }

    fn user() -> String {
        json_200(&json!({"uniqueId": "me", "fullName": "Y", "email": "y@dev.pro"}).to_string())
    }

    fn assigned() -> String {
        json_200(
            &json!({"uniqueId": "me", "projects": [
                {"uniqueId": "id-inveniam", "shortName": "Inveniam SOW #5"},
                {"uniqueId": "id-presales", "shortName": "Presales"}
            ]})
            .to_string(),
        )
    }

    #[derive(Default)]
    struct Recorder {
        out: Vec<String>,
        err: Vec<String>,
    }

    impl Console for Recorder {
        fn out(&mut self, line: &str) {
            self.out.push(line.to_string());
        }
        fn err(&mut self, line: &str) {
            self.err.push(line.to_string());
        }
    }

    struct Run {
        result: Result<()>,
        io: Recorder,
        file: String,
        dir: TempDir,
    }

    async fn run(args: MappingAddArgs, responses: Vec<String>) -> (Run, StubServer) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, CONFIG).unwrap();
        let stub = StubServer::start(responses);
        let timeout = Duration::from_secs(2);
        let portal =
            TtApiClient::with_base_url("cookie", &stub.base_url, timeout, timeout).unwrap();
        let mut io = Recorder::default();
        let today = NaiveDate::from_ymd_opt(2026, 10, 8).unwrap();
        let result = add(&portal, &path, &args, today, &mut io).await;
        let file = std::fs::read_to_string(&path).unwrap();
        (
            Run {
                result,
                io,
                file,
                dir,
            },
            stub,
        )
    }

    #[test]
    fn the_item_lands_after_the_last_mapping_and_before_the_next_blocks_comment() {
        assert_eq!(insert_mapping(CONFIG, &presales()).unwrap(), EDITED);
    }

    #[test]
    fn a_block_at_the_end_of_the_file_takes_the_item_at_its_end() {
        let text = "chrono_api: \"x\"\nvault_path: \"/v\"\nmappings:\n    - chrono_project: \"A\"\n      devpro_project: \"B\"\n      billability: \"Billable\"\n";
        let edited = insert_mapping(text, &presales()).unwrap();
        assert_eq!(
            edited,
            format!(
                "{}    - chrono_project: \"Presales - DevPro - Work\"\n      devpro_project: \"Presales\"\n      billability: \"NonBillable\"\n",
                text
            )
        );
        assert_eq!(config::parse(&edited).unwrap().mappings[1], presales());
    }

    #[test]
    fn quotes_and_backslashes_in_a_name_survive_the_round_trip() {
        let mapping = ProjectMapping {
            chrono_project: r#"Odd "name" \ here - DevPro - Work"#.to_string(),
            ..presales()
        };
        let edited = insert_mapping(CONFIG, &mapping).unwrap();
        assert_eq!(config::parse(&edited).unwrap().mappings[2], mapping);
    }

    #[test]
    fn a_config_without_a_block_header_is_refused() {
        let error = insert_mapping("mappings: []\n", &presales()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the config has no `mappings:` block in column 0"
        );
    }

    #[tokio::test]
    async fn an_assigned_project_is_added_and_the_comments_stay() {
        let (run, stub) = run(
            args("Presales - DevPro - Work", "Presales", "NonBillable"),
            vec![user(), assigned()],
        )
        .await;

        run.result.unwrap();
        let requests = stub.requests();
        assert!(
            requests[1].target.contains("dateFrom=2026-10-08"),
            "{}",
            requests[1].target
        );
        assert_eq!(run.file, EDITED);
        assert!(!run.dir.path().join("config.yaml.tmp").exists());
        assert_eq!(run.io.out.len(), 1);
        assert!(
            run.io.out[0]
                .starts_with("✓ Mapped Presales - DevPro - Work → Presales (NonBillable) in "),
            "{}",
            run.io.out[0]
        );
    }

    #[tokio::test]
    async fn a_project_not_assigned_today_is_refused_and_the_file_is_untouched() {
        let (run, stub) = run(
            args("Nordis - DevPro - Work", "Nordis", "Billable"),
            vec![user(), assigned()],
        )
        .await;

        stub.requests();
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "Nordis is not assigned to you on 2026-10-08. Assigned: Inveniam SOW #5, Presales"
        );
        assert_eq!(run.file, CONFIG);
    }

    #[tokio::test]
    async fn a_chrono_project_already_mapped_is_refused_before_any_request() {
        let (run, stub) = run(
            args("AI Practices - DevPro - Work", "Presales", "NonBillable"),
            vec![],
        )
        .await;

        assert!(stub.requests().is_empty());
        let message = run.result.unwrap_err().to_string();
        assert!(
            message.starts_with(
                "AI Practices - DevPro - Work is already mapped to AI Practices as NonBillable in "
            ),
            "{message}"
        );
        assert_eq!(run.file, CONFIG);
    }

    #[tokio::test]
    async fn a_billability_outside_the_two_values_is_refused_before_any_request() {
        let (run, stub) = run(
            args("Presales - DevPro - Work", "Presales", "billable"),
            vec![],
        )
        .await;

        assert!(stub.requests().is_empty());
        assert_eq!(
            run.result.unwrap_err().to_string(),
            "--billability must be Billable or NonBillable, not `billable`"
        );
        assert_eq!(run.file, CONFIG);
    }

    #[test]
    fn an_edit_that_does_not_read_back_leaves_the_file_as_it_was() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, CONFIG).unwrap();
        let before = config::parse(CONFIG).unwrap().mappings;

        let error = replace_checked(&path, CONFIG, &before, &presales()).unwrap_err();

        assert!(
            format!("{error:#}").contains("does not read back"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CONFIG);
        assert!(!dir.path().join("config.yaml.tmp").exists());
    }
}
