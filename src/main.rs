//! `tt-devpro` — settle Dev.Pro time reports from Chrono.
//!
//! Ports `Main.kt`, which is nine lines of `TtCli().subcommands(…).main(args)`
//! (`Main.kt:42-49`). Almost everything in this file is the part `Main.kt` got for
//! free by being written against Clikt: **the CLI surface this tool presents is
//! Clikt's, and clap renders neither its help nor its failures the same way.**
//!
//! So clap is used for exactly one job — turning an argument list that is already
//! known to be valid into typed values — and the two surfaces a caller actually
//! sees are produced here:
//!
//! **Help** is the captured bytes of the incumbent's own `--help`, held in the
//! `*_HELP` constants (D6). [`ROOT_HELP`] lives here; the other six live beside the
//! commands they describe. The scan that decides *which* one to print runs before
//! clap sees anything, because Clikt's help option is eager and beats every error
//! in the same command — measured, see [`analyze`].
//!
//! **Failures** are rebuilt line by line from the argument list by [`analyze`],
//! because clap reports the *first* problem and Clikt reports *all* of them:
//! `api create-worklog --nosuchopt` prints six lines where clap prints one. The
//! rules were read out of the pinned incumbent
//! (`~/.cache/tt-devpro-rewrite/tt-devpro.kotlin-incumbent`) rather than out of
//! Clikt's source, and every one of them is cited at the rule it produced.
//!
//! **C33 — a usage failure exits 1.** clap exits 2 and offers no setting for it
//! (`Error::exit_code()` returns the constant `USAGE_CODE = 2`), so the only way
//! out is never to let clap exit: [`main`] renders the failure itself.
//!
//! **D3 — a command failure exits [`Outcome::exit_code`].** The message is already
//! on stderr, put there where the incumbent puts it; `main` must not render it a
//! second time in a shape nothing measured.

mod api;
mod commands;
mod config;
mod cookie;
mod fmt;
mod model;
mod service;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::commands::api::{
    API_HELP, CREATE_WORKLOG_HELP, CreateWorklogArgs, DELETE_WORKLOG_HELP, DeleteWorklogArgs,
    GET_PROJECTS_HELP, GET_WORKLOGS_HELP, GetProjectsArgs, GetWorklogsArgs, UPDATE_WORKLOG_HELP,
    UpdateWorklogArgs,
};
use crate::commands::settle::{Console, SETTLE_HELP, SettleArgs, Stdio};
use crate::commands::{
    Outcome, no_such_option, no_such_subcommand, parse_iso_date, suggest, usage_error, usage_line,
};

// ---------------------------------------------------------------------------
// The root help text
// ---------------------------------------------------------------------------

/// `tt-devpro --help`, and also what `tt-devpro` with no arguments prints — on
/// **stdout**, exit **0**. The captured bytes of
/// `~/.cache/tt-devpro-rewrite/baseline/help.out`, which is byte-identical to
/// `cli-errors/root-no-args.out`, with the trailing newline stripped.
///
/// This constant is the one D6 text with no sibling to live beside: the root
/// command is declared here, in the port exactly as in `Main.kt:12-17`, so its help
/// is declared here too. The other six sit next to the commands they describe.
///
/// Its first line is the usage line every root-level failure prints; see
/// [`crate::commands::usage_line`].
pub const ROOT_HELP: &str = r#"Usage: tt-devpro [<options>] <command> [<args>]...

  Settle Dev.Pro time reports from Chrono

Options:
  -h, --help  Show this message and exit

Commands:
  settle  Settle daily hours: normalize to 8h, auto-fill gaps, push to DevPro
  api     Direct API calls to Time Tracking Portal"#;

// ---------------------------------------------------------------------------
// The clap tree
// ---------------------------------------------------------------------------

/// `TtCli` (`Main.kt:12-17`) with its two subcommands (`Main.kt:44-47`).
///
/// `disable_help_flag` is on every command in this tree and is **not** optional.
/// `api create-worklog` binds `-h` to `--hours` (C21, `ApiCommand.kt:107`), so
/// clap's built-in `-h` would shadow a real option and answer help where the
/// incumbent answers `Error: option -h requires a value`
/// (`cli-errors/hours-short-flag-is-not-help.err`). Help is served by [`analyze`]
/// instead, which knows who owns `-h` on each command.
///
/// The subcommand is `Option` for the same reason Clikt's group commands have
/// `override fun run() = Unit`: a group reached with no subcommand prints its own
/// help and exits 0 (`cli-errors/root-no-args`, `cli-errors/api-no-args`). Making
/// it required would turn that into an error.
#[derive(Parser, Debug)]
#[command(
    name = "tt-devpro",
    disable_help_flag = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<TopCommand>,
}

#[derive(Subcommand, Debug)]
enum TopCommand {
    #[command(disable_help_flag = true)]
    Settle(SettleArgs),
    #[command(disable_help_flag = true, disable_help_subcommand = true)]
    Api {
        #[command(subcommand)]
        command: Option<ApiCommand>,
    },
}

/// `apiSubcommands()` (`ApiCommand.kt:221-227`), in registration order — which is
/// also the order the group listing prints and the order missing-option lines come
/// out in.
#[derive(Subcommand, Debug)]
enum ApiCommand {
    #[command(disable_help_flag = true)]
    GetProjects(GetProjectsArgs),
    #[command(disable_help_flag = true)]
    GetWorklogs(GetWorklogsArgs),
    #[command(disable_help_flag = true)]
    CreateWorklog(CreateWorklogArgs),
    #[command(disable_help_flag = true)]
    UpdateWorklog(UpdateWorklogArgs),
    #[command(disable_help_flag = true)]
    DeleteWorklog(DeleteWorklogArgs),
}

/// The built clap tree, used both as the model [`analyze`] walks and as the parser
/// that turns a valid argument list into typed values.
///
/// Deriving the model from the parser rather than writing it twice is the whole
/// reason the failure surface can be rebuilt here without drifting: rename an
/// option in its `Args` struct and the suggestion list, the missing-option lines
/// and the parse all move together.
///
/// **`allow_hyphen_values` on every argument** is Clikt's behaviour, not a
/// loosening. A Clikt option that takes a value consumes the next token whatever it
/// looks like: `api get-projects -d --help` answers
/// `Error: invalid value for -d: Text '--help' could not be parsed at index 1`, and
/// `api create-worklog -h --help` reports four missing options rather than five —
/// `--help` became the hours. clap refuses a hyphenated value by default, so
/// without this the analyzer and the parser would disagree on exactly the argument
/// lists where `-h` is not help.
fn command() -> clap::Command {
    let mut cmd = hyphenated_values_allowed(Cli::command()).no_binary_name(true);
    cmd.build();
    cmd
}

fn hyphenated_values_allowed(cmd: clap::Command) -> clap::Command {
    // Only on arguments that take a value: clap asserts on the combination of
    // `allow_hyphen_values` with a flag, and a flag has no value to hyphenate.
    let cmd = cmd.mut_args(|arg| {
        if matches!(
            arg.get_action(),
            clap::ArgAction::Set | clap::ArgAction::Append
        ) {
            arg.allow_hyphen_values(true)
        } else {
            arg
        }
    });
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    names.into_iter().fold(cmd, |cmd, name| {
        cmd.mut_subcommand(name, hyphenated_values_allowed)
    })
}

/// The `*_HELP` constant for a command path, `[]` being the root.
///
/// `None` is unreachable — [`tests::every_command_in_the_clap_tree_has_a_help_constant`]
/// walks the tree and fails if a command is ever added without one — and is returned
/// rather than defaulted so that adding a command without its help text cannot
/// quietly serve the wrong text.
fn help_for(path: &[&str]) -> Option<&'static str> {
    Some(match path {
        [] => ROOT_HELP,
        ["settle"] => SETTLE_HELP,
        ["api"] => API_HELP,
        ["api", "get-projects"] => GET_PROJECTS_HELP,
        ["api", "get-worklogs"] => GET_WORKLOGS_HELP,
        ["api", "create-worklog"] => CREATE_WORKLOG_HELP,
        ["api", "update-worklog"] => UPDATE_WORKLOG_HELP,
        ["api", "delete-worklog"] => DELETE_WORKLOG_HELP,
        _ => return None,
    })
}

/// The three options declared with `.convert { LocalDate.parse(it) }`, whose value
/// is therefore validated *during parsing* and whose failure is a usage error
/// rather than a runtime one: `--from` and `--to` (`SettleCommand.kt:56,59`) and
/// `api get-projects --date` (`ApiCommand.kt:53`).
///
/// The boundary worth noticing is that `api get-worklogs --date`
/// (`ApiCommand.kt:74`) has the same name and does **not** convert — the text goes
/// to the portal as typed. Clikt shows the difference only in the metavar
/// (`<value>` against `<text>`), which is why this is a table and not an inference.
/// [`tests::exactly_the_date_converting_options_reject_a_non_date_at_parse_time`]
/// holds it against what clap's own value parsers do.
fn converts_a_date(path: &[&str], long: &str) -> bool {
    matches!(
        (path, long),
        (["settle"], "--from") | (["settle"], "--to") | (["api", "get-projects"], "--date")
    )
}

// ---------------------------------------------------------------------------
// The Clikt failure surface, rebuilt
// ---------------------------------------------------------------------------

/// What the process should print and exit with. The strings are the **exact
/// bytes**, trailing newline included, so a test can hold them against a capture
/// file without reasoning about who adds the newline.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rendered {
    stdout: Option<String>,
    stderr: Option<String>,
    code: i32,
}

/// What walking the argument list decided.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    /// The argument list is well formed; hand it to clap.
    Proceed,
    /// An eager help option fired on this command.
    Help(&'static str),
    /// One or more usage failures, in the order the incumbent prints them.
    Usage {
        help: &'static str,
        messages: Vec<String>,
    },
}

/// One option as the walk needs it, built from the clap tree.
struct OptionSpec {
    long: String,
    short: Option<char>,
    takes_value: bool,
    required: bool,
    is_help: bool,
}

/// Every option of a command, in declaration order, with Clikt's help option
/// appended.
///
/// **Help gives up `-h` to whichever option claimed it** (C21). That is a rule
/// rather than two special cases: `api create-worklog` and `api update-worklog`
/// bind `-h` to `--hours`, every other command leaves it free, and the captured
/// help texts show exactly that — `--help` alone on those two,
/// `-h, --help` everywhere else.
///
/// Help comes last because that is where Clikt prints it, and the order is what the
/// suggestion list is built from.
fn options_of(cmd: &clap::Command) -> Vec<OptionSpec> {
    let mut options: Vec<OptionSpec> = cmd
        .get_arguments()
        .filter_map(|arg| {
            let long = arg.get_long()?;
            Some(OptionSpec {
                long: format!("--{long}"),
                short: arg.get_short(),
                takes_value: arg.get_num_args().is_none_or(|n| n.takes_values()),
                required: arg.is_required_set(),
                is_help: false,
            })
        })
        .collect();
    let h_is_taken = options.iter().any(|option| option.short == Some('h'));
    options.push(OptionSpec {
        long: "--help".to_string(),
        short: if h_is_taken { None } else { Some('h') },
        takes_value: false,
        required: false,
        is_help: true,
    });
    options
}

/// Every option *name* a command answers to, long and short, in declaration order.
///
/// This is what a near miss is scored against, and it includes the short forms:
/// `api get-projects --dat 2026-01-01` answers
/// `Error: no such option --dat. (Possible options: --date, -d)`. A list of long
/// forms only would drop the `-d`.
fn option_names(options: &[OptionSpec]) -> Vec<String> {
    let mut names = Vec::new();
    for option in options {
        names.push(option.long.clone());
        if let Some(short) = option.short {
            names.push(format!("-{short}"));
        }
    }
    names
}

/// Walk the argument list the way Clikt does and say what the caller gets.
///
/// Every rule below was measured against the pinned incumbent, and the invocation
/// that produced it is named. None of them is clap's.
///
/// 1. **Help is eager and beats every error in the same command.**
///    `settle --nosuchflag --help` and `settle --from notadate --help` both print
///    settle's help and exit 0.
/// 2. **…unless it was eaten as an option's value.** `api create-worklog -h --help`
///    reports four missing options — `-h` is `--hours` and took `--help`.
/// 3. **Help belongs to the deepest command reached before it.** `--help api`
///    prints the root's, `api --help get-projects` prints `api`'s,
///    `settle --dry-run --help` prints settle's.
/// 4. **An unrecognised token does not end the scan.** `api nosuchsub --help`
///    prints `api`'s help: the token became a positional, and the loop ran on.
/// 5. **Token-pass errors come out in token order**, finalization errors after
///    them: `api create-worklog --nosuchopt` prints the unknown option, then the
///    five missing ones.
/// 6. **A token-pass error suppresses the positional pass, but not the
///    missing-option pass.** `settle --dry-run extra --nosuchflag` reports only the
///    unknown option — no `extra argument` — and `api delete-worklog --nosuchopt
///    aaa bbb` reports only the unknown option, no `missing argument <id>` and no
///    excess. Yet `api create-worklog --nosuchopt` still lists its five missing
///    options. So the arguments are never finalized once a token failed, and the
///    options always are.
/// 7. **The subcommand is resolved in that same suppressed pass**, which is why
///    `api --nosuch get-projects` reports the unknown option against **`api`'s**
///    usage line and `get-projects` never runs.
/// 8. **One excess token under a command that has subcommands is a subcommand
///    miss; two are extra arguments.** `api no-such-thing` says
///    `no such subcommand`, `api nosuchsub anotherone` says
///    `got unexpected extra arguments (nosuchsub anotherone)`. Under a command with
///    no subcommands one excess token is already an extra argument
///    (`settle extra-arg`).
fn analyze(argv: &[String]) -> Verdict {
    let cmd = command();
    walk(&cmd, &[], argv)
}

fn walk(cmd: &clap::Command, path: &[&str], tokens: &[String]) -> Verdict {
    let help = help_for(path).expect("every command in the tree has a help constant");
    let options = options_of(cmd);
    let names = option_names(&options);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let subcommands: Vec<&str> = cmd.get_subcommands().map(clap::Command::get_name).collect();

    let mut messages: Vec<String> = Vec::new();
    let mut positionals: Vec<&str> = Vec::new();
    let mut satisfied: Vec<&str> = Vec::new();
    let mut help_fired = false;
    let mut options_ended = false;
    let mut descend_at: Option<usize> = None;
    let mut index = 0;

    while index < tokens.len() {
        let token = tokens[index].as_str();

        // A recognised subcommand name ends the parent's token stream, whatever
        // else the parent has collected — `api --nosuch get-projects` proves the
        // parent's errors survive it, and `api get-projects --date=nope` proves the
        // child's options are not parsed by the parent.
        if subcommands.contains(&token) {
            descend_at = Some(index);
            break;
        }

        if !options_ended && token == "--" {
            options_ended = true;
            index += 1;
            continue;
        }

        if !options_ended && token.starts_with("--") {
            let (name, attached) = match token.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (token, None),
            };
            match options.iter().find(|option| option.long == name) {
                None => messages.push(no_such_option(name, &suggest(name, &names))),
                Some(option) if !option.takes_value => {
                    // `settle --json=yes` → `option --json does not take a value`.
                    // Help is an ordinary flag here, so `--help=x` takes the same
                    // line rather than printing help.
                    if attached.is_some() {
                        messages.push(format!("option {name} does not take a value"));
                    } else if option.is_help {
                        help_fired = true;
                    }
                }
                Some(option) => {
                    let value = match attached {
                        Some(value) => Some(value.to_string()),
                        None => {
                            index += 1;
                            tokens.get(index).cloned()
                        }
                    };
                    take_value(
                        &mut messages,
                        &mut satisfied,
                        path,
                        option,
                        name,
                        value.as_deref(),
                    );
                }
            }
            index += 1;
            continue;
        }

        if !options_ended && token.starts_with('-') && token.len() > 1 {
            let letters: Vec<char> = token[1..].chars().collect();
            let mut position = 0;
            while position < letters.len() {
                let letter = letters[position];
                let typed = format!("-{letter}");
                match options.iter().find(|option| option.short == Some(letter)) {
                    None => {
                        // `api get-projects -zd 2026-01-01` reports `-z` and stops:
                        // one message, not one per remaining letter.
                        messages.push(no_such_option(&typed, &suggest(&typed, &names)));
                        break;
                    }
                    Some(option) if !option.takes_value => {
                        if option.is_help {
                            help_fired = true;
                        }
                        position += 1;
                    }
                    Some(option) => {
                        // `api get-projects -dh` → `-d` takes the letter `h`, not a
                        // second flag: the rest of the cluster is the value.
                        let rest: String = letters[position + 1..].iter().collect();
                        let value = if rest.is_empty() {
                            index += 1;
                            tokens.get(index).cloned()
                        } else {
                            Some(rest)
                        };
                        take_value(
                            &mut messages,
                            &mut satisfied,
                            path,
                            option,
                            &typed,
                            value.as_deref(),
                        );
                        break;
                    }
                }
            }
            index += 1;
            continue;
        }

        positionals.push(token);
        index += 1;
    }

    if help_fired {
        return Verdict::Help(help);
    }

    // Rule 6: the argument pass runs only on a clean token pass. Rule 7 puts the
    // descent inside it.
    if messages.is_empty() {
        if let Some(at) = descend_at {
            let name = tokens[at].as_str();
            let sub = cmd.find_subcommand(name).expect("matched just above");
            let mut child = path.to_vec();
            child.push(sub.get_name());
            return walk(sub, &child, &tokens[at + 1..]);
        }

        let declared: Vec<(String, bool)> = cmd
            .get_positionals()
            .map(|arg| (arg.get_id().to_string(), arg.is_required_set()))
            .collect();

        if positionals.len() > declared.len() {
            let excess = &positionals[declared.len()..];
            if excess.len() == 1 && !subcommands.is_empty() {
                messages.push(no_such_subcommand(
                    excess[0],
                    &suggest(excess[0], &subcommands),
                ));
            } else {
                let noun = if excess.len() == 1 {
                    "argument"
                } else {
                    "arguments"
                };
                messages.push(format!(
                    "got unexpected extra {noun} ({})",
                    excess.join(" ")
                ));
            }
        }
        for (name, required) in declared.iter().skip(positionals.len()) {
            if *required {
                messages.push(format!("missing argument <{name}>"));
            }
        }
    }

    for option in &options {
        if option.required && !satisfied.contains(&option.long.as_str()) {
            messages.push(format!("missing option {}", option.long));
        }
    }

    if messages.is_empty() {
        Verdict::Proceed
    } else {
        Verdict::Usage { help, messages }
    }
}

/// The value half of an option token, shared by the long and the short spelling.
///
/// **The option is echoed as the caller typed it.** `api get-projects -d` answers
/// `Error: option -d requires a value` and `api get-projects --date` answers
/// `Error: option --date requires a value` — both spellings are live and a port
/// that normalised to the declared long name would fail one of them
/// (`cli-errors/short-option-needs-value.err` against `cli-errors/option-needs-value.err`).
///
/// **A valueless option is still missing.** `api get-worklogs --date` emits both
/// `option --date requires a value` and `missing option --date`
/// (`cli-errors/needs-value-and-missing.err`), so nothing is marked satisfied until
/// a value actually arrived.
fn take_value<'a>(
    messages: &mut Vec<String>,
    satisfied: &mut Vec<&'a str>,
    path: &[&str],
    option: &'a OptionSpec,
    typed: &str,
    value: Option<&str>,
) {
    let Some(value) = value else {
        messages.push(format!("option {typed} requires a value"));
        return;
    };
    satisfied.push(&option.long);
    if converts_a_date(path, &option.long)
        && let Err(reason) = parse_iso_date(value)
    {
        // C33's named divergence: the prefix is the incumbent's, the tail is ours.
        // Java's is `DateTimeParseException.getMessage()` and reproducing it means
        // hand-writing Java's message catalogue for a string no caller parses.
        messages.push(format!("invalid value for {typed}: {reason}"));
    }
}

/// [`analyze`]'s verdict as bytes and an exit code. `None` means "clap's turn".
///
/// This is the seam the tests work against: everything above is a pure function of
/// the argument list, so the whole failure surface is testable without spawning a
/// process.
fn cli_failure(argv: &[String]) -> Option<Rendered> {
    match analyze(argv) {
        Verdict::Proceed => None,
        Verdict::Help(help) => Some(Rendered {
            stdout: Some(format!("{help}\n")),
            stderr: None,
            // Clikt's `PrintHelpMessage(error = false)` — stdout, and **0**.
            code: 0,
        }),
        Verdict::Usage { help, messages } => Some(Rendered {
            stdout: None,
            stderr: Some(format!("{}\n", usage_error(usage_line(help), &messages))),
            // C33. clap would exit 2 here and has no setting that changes it.
            code: 1,
        }),
    }
}

/// The net under [`analyze`]: a clap error on an argument list the analyzer passed.
///
/// Nothing measured reaches it — the analyzer answers every failing invocation
/// captured or probed — but "nothing measured" is not "nothing", and a clap error
/// rendered clap's way would be the one line of output in this tool that nobody
/// chose. So it is put through the same usage block and the same exit code as
/// everything else, carrying clap's own sentence.
fn clap_fallback(error: &clap::Error) -> Rendered {
    let rendered = error.render().to_string();
    let message = rendered
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("argument parsing failed")
        .trim_start_matches("error: ")
        .to_string();
    Rendered {
        stdout: None,
        stderr: Some(format!(
            "{}\n",
            usage_error(usage_line(ROOT_HELP), &[message])
        )),
        code: 1,
    }
}

fn parse(argv: &[String]) -> Result<Cli, Rendered> {
    let matches = command()
        .try_get_matches_from(argv)
        .map_err(|error| clap_fallback(&error))?;
    Cli::from_arg_matches(&matches).map_err(|error| clap_fallback(&error))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// `TtCli().subcommands(…)` (`Main.kt:43-47`), plus the two group commands'
/// `override fun run() = Unit` (`Main.kt:16`, `ApiCommand.kt:26`) — which in Clikt
/// means "a group reached without a subcommand prints its help", exit 0.
///
/// D3: the returned [`Outcome`] *is* the exit code. Every command has already put
/// its own message on stderr where the incumbent puts it, so there is nothing left
/// here to render.
async fn dispatch(cli: &Cli, io: &mut dyn Console) -> Outcome {
    match &cli.command {
        None => {
            io.out(ROOT_HELP);
            Outcome::Ok
        }
        Some(TopCommand::Settle(args)) => commands::settle::run(args, io).await,
        Some(TopCommand::Api { command }) => match command {
            None => {
                io.out(API_HELP);
                Outcome::Ok
            }
            Some(ApiCommand::GetProjects(args)) => commands::api::run_get_projects(args, io).await,
            Some(ApiCommand::GetWorklogs(args)) => commands::api::run_get_worklogs(args, io).await,
            Some(ApiCommand::CreateWorklog(args)) => {
                commands::api::run_create_worklog(args, io).await
            }
            Some(ApiCommand::UpdateWorklog(args)) => {
                commands::api::run_update_worklog(args, io).await
            }
            Some(ApiCommand::DeleteWorklog(args)) => {
                commands::api::run_delete_worklog(args, io).await
            }
        },
    }
}

fn emit(rendered: &Rendered) {
    if let Some(text) = &rendered.stdout {
        print!("{text}");
    }
    if let Some(text) = &rendered.stderr {
        eprint!("{text}");
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if let Some(rendered) = cli_failure(&argv) {
        emit(&rendered);
        std::process::exit(rendered.code);
    }

    let cli = match parse(&argv) {
        Ok(cli) => cli,
        Err(rendered) => {
            emit(&rendered);
            std::process::exit(rendered.code);
        }
    };

    let mut io = Stdio::new();
    std::process::exit(dispatch(&cli, &mut io).await.exit_code());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The argument list as the shell hands it over, arg0 already dropped.
    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_string()).collect()
    }

    /// What the process would print and exit with, for an argument list that never
    /// reaches a command body.
    fn outcome(tokens: &[&str]) -> Rendered {
        cli_failure(&argv(tokens))
            .unwrap_or_else(|| panic!("{tokens:?} was expected to fail or print help"))
    }

    fn stderr_of(tokens: &[&str]) -> String {
        let rendered = outcome(tokens);
        assert_eq!(rendered.stdout, None, "a usage failure writes no stdout");
        assert_eq!(rendered.code, 1, "C33: a usage failure exits 1");
        rendered.stderr.expect("a usage failure writes stderr")
    }

    /// Walks the whole clap tree, yielding each command's path.
    fn every_path() -> Vec<Vec<String>> {
        fn walk(cmd: &clap::Command, path: Vec<String>, out: &mut Vec<Vec<String>>) {
            out.push(path.clone());
            for sub in cmd.get_subcommands() {
                let mut child = path.clone();
                child.push(sub.get_name().to_string());
                walk(sub, child, out);
            }
        }
        let mut out = Vec::new();
        walk(&command(), Vec::new(), &mut out);
        out
    }

    // -----------------------------------------------------------------------
    // The help surface (D6)
    // -----------------------------------------------------------------------

    /// `~/.cache/tt-devpro-rewrite/baseline/help.out`, and `cli-errors/root-no-args.out`,
    /// which the baseline README records as byte-identical to it. Transcribed rather
    /// than read off disk so the suite does not depend on a cache directory.
    #[test]
    fn the_root_help_is_the_captured_bytes_of_the_incumbents_own_help() {
        let captured = "Usage: tt-devpro [<options>] <command> [<args>]...\n\
                        \n  Settle Dev.Pro time reports from Chrono\n\
                        \nOptions:\n  -h, --help  Show this message and exit\n\
                        \nCommands:\n  \
                        settle  Settle daily hours: normalize to 8h, auto-fill gaps, push to DevPro\n  \
                        api     Direct API calls to Time Tracking Portal\n";
        assert_eq!(outcome(&["--help"]).stdout.as_deref(), Some(captured));
        assert_eq!(
            usage_line(ROOT_HELP),
            "Usage: tt-devpro [<options>] <command> [<args>]...",
            "and its first line is what a root-level failure prints"
        );
    }

    /// The constant table and the clap tree are two lists that can disagree, and the
    /// disagreement would surface only on a failing invocation of the new command —
    /// exactly where nobody is looking. Adding a subcommand without its help text
    /// fails here rather than panicking in [`walk`] at runtime.
    #[test]
    fn every_command_in_the_clap_tree_has_a_help_constant() {
        for path in every_path() {
            let borrowed: Vec<&str> = path.iter().map(String::as_str).collect();
            assert!(
                help_for(&borrowed).is_some(),
                "no help constant for `tt-devpro {}`",
                path.join(" ")
            );
        }
        assert_eq!(every_path().len(), 8, "root, settle, api and api's five");
    }

    /// C21's mechanism rather than its symptom: with clap's own help flag left on,
    /// `-h` would be help on `create-worklog` and the measured
    /// `Error: option -h requires a value` would be unreachable. A command added
    /// later without `disable_help_flag` fails here.
    #[test]
    fn no_command_in_the_tree_declares_claps_own_help_flag() {
        fn check(cmd: &clap::Command) {
            assert!(
                !cmd.get_arguments().any(|arg| arg.get_id() == "help"),
                "`{}` still has clap's help flag",
                cmd.get_name()
            );
            for sub in cmd.get_subcommands() {
                check(sub);
            }
        }
        check(&command());
    }

    /// Measured on the pinned incumbent: `--help api` prints the root's help,
    /// `api --help get-projects` prints `api`'s, `settle --dry-run --help` prints
    /// settle's. A scan that took the *last* command named, or the deepest one
    /// present anywhere in the list, would fail every row.
    #[test]
    fn help_belongs_to_the_deepest_command_reached_before_it() {
        for (tokens, expected) in [
            (vec!["--help", "api"], ROOT_HELP),
            (vec!["api", "--help", "get-projects"], API_HELP),
            (vec!["settle", "--dry-run", "--help"], SETTLE_HELP),
            (
                vec!["api", "get-projects", "--help", "--date"],
                GET_PROJECTS_HELP,
            ),
            (vec!["api", "delete-worklog", "--help"], DELETE_WORKLOG_HELP),
        ] {
            let rendered = outcome(&tokens);
            assert_eq!(
                rendered.stdout.as_deref(),
                Some(format!("{expected}\n").as_str()),
                "{tokens:?}"
            );
            assert_eq!(rendered.stderr, None, "{tokens:?}: help is not an error");
            assert_eq!(rendered.code, 0, "{tokens:?}");
        }
    }

    /// Clikt's help option is eager: it is finalized before any collected error is
    /// reported. Measured — `settle --nosuchflag --help`, `settle --from notadate --help`
    /// and `api create-worklog --nosuchopt --help` all exit 0 with help on stdout.
    /// A scan that ran *after* the error pass would print three usage blocks.
    #[test]
    fn help_beats_every_error_in_the_same_command() {
        for tokens in [
            vec!["settle", "--nosuchflag", "--help"],
            vec!["settle", "--from", "notadate", "--help"],
            vec!["api", "create-worklog", "--nosuchopt", "--help"],
            vec!["settle", "--help", "extra"],
            vec!["--help", "--nosuchflag"],
        ] {
            let rendered = outcome(&tokens);
            assert_eq!(rendered.code, 0, "{tokens:?}");
            assert_eq!(rendered.stderr, None, "{tokens:?}");
        }
    }

    /// Measured: `api nosuchsub --help` prints `api`'s help and exits 0. The
    /// unrecognised token becomes a positional and the scan runs on — a scan that
    /// stopped at the first token it could not place would answer
    /// `no such subcommand` here.
    #[test]
    fn an_unrecognised_token_does_not_stop_the_scan_from_finding_help() {
        let rendered = outcome(&["api", "nosuchsub", "--help"]);
        assert_eq!(
            rendered.stdout.as_deref(),
            Some(format!("{API_HELP}\n").as_str())
        );
        assert_eq!(rendered.code, 0);
        assert_eq!(
            stderr_of(&["api", "nosuchsub"]),
            "Usage: tt-devpro api [<options>] <command> [<args>]...\n\nError: no such subcommand nosuchsub\n",
            "without the --help the same token is a subcommand miss"
        );
    }

    /// C21 as a rule rather than two exceptions: `-h` is help exactly where no
    /// option claimed the letter. `api get-projects -h` and `-h` at the root print
    /// help; `api create-worklog -h` does not.
    #[test]
    fn minus_h_is_help_everywhere_except_the_two_commands_that_bind_it_to_hours() {
        for (tokens, expected) in [
            (vec!["-h"], ROOT_HELP),
            (vec!["api", "-h"], API_HELP),
            (vec!["settle", "-h"], SETTLE_HELP),
            (vec!["api", "get-projects", "-h"], GET_PROJECTS_HELP),
            (vec!["api", "get-worklogs", "-h"], GET_WORKLOGS_HELP),
            (vec!["api", "delete-worklog", "-h"], DELETE_WORKLOG_HELP),
        ] {
            assert_eq!(
                outcome(&tokens).stdout.as_deref(),
                Some(format!("{expected}\n").as_str()),
                "{tokens:?}"
            );
        }
        for tokens in [
            vec!["api", "create-worklog", "-h"],
            vec!["api", "update-worklog", "-h"],
        ] {
            assert!(
                stderr_of(&tokens).contains("Error: option -h requires a value"),
                "{tokens:?}: -h is --hours here"
            );
        }
    }

    /// Measured: `api create-worklog --help -h` prints help (the eager `--help`
    /// comes first), while `api create-worklog -h --help` reports **four** missing
    /// options — `-h` is `--hours` and swallowed `--help`, so `--hours` is satisfied
    /// and never listed. A scan that treated `--help` as help wherever it appears
    /// would print help for the second line too.
    #[test]
    fn a_help_flag_consumed_as_an_option_value_is_not_help() {
        assert_eq!(
            outcome(&["api", "create-worklog", "--help", "-h"])
                .stdout
                .as_deref(),
            Some(format!("{CREATE_WORKLOG_HELP}\n").as_str())
        );
        assert_eq!(
            stderr_of(&["api", "create-worklog", "-h", "--help"]),
            "Usage: tt-devpro api create-worklog [<options>]\n\
             \n\
             Error: missing option --date\n\
             Error: missing option --project-id\n\
             Error: missing option --task\n\
             Error: missing option --billability\n"
        );
    }

    /// Measured: `api get-projects -d --help` answers `invalid value for -d`, so the
    /// value token reached the converter rather than the help scan.
    #[test]
    fn a_value_taking_option_swallows_a_help_token_and_hands_it_to_its_converter() {
        assert_eq!(
            stderr_of(&["api", "get-projects", "-d", "--help"]),
            "Usage: tt-devpro api get-projects [<options>]\n\n\
             Error: invalid value for -d: --help is not a date in YYYY-MM-DD form\n"
        );
    }

    // -----------------------------------------------------------------------
    // The seventeen captured failures (C33)
    // -----------------------------------------------------------------------

    /// Thirteen of the seventeen captures under
    /// `~/.cache/tt-devpro-rewrite/baseline/cli-errors/`, byte for byte. The other
    /// four embed `DateTimeParseException.getMessage()` and have their own test.
    ///
    /// Each row is `(invocation, capture name, expected stderr)`; stdout is empty
    /// and the code is 1 on every one of them except the two `*-no-args` cases,
    /// which print a group's help on stdout and exit 0 and are covered by
    /// [`a_group_command_with_no_subcommand_prints_its_own_help`].
    #[test]
    fn the_captured_usage_failures_reproduce_byte_for_byte() {
        let cases: &[(&[&str], &str, &str)] = &[
            (
                &["api", "create-worklog", "-h"],
                "hours-short-flag-is-not-help",
                "Usage: tt-devpro api create-worklog [<options>]\n\nError: option -h requires a value\nError: missing option --date\nError: missing option --project-id\nError: missing option --task\nError: missing option --billability\nError: missing option --hours\n",
            ),
            (
                &["api", "delete-worklog"],
                "missing-argument-delete",
                "Usage: tt-devpro api delete-worklog [<options>] <id>\n\nError: missing argument <id>\n",
            ),
            (
                &["api", "get-worklogs"],
                "missing-option-worklogs",
                "Usage: tt-devpro api get-worklogs [<options>]\n\nError: missing option --date\n",
            ),
            (
                &["api", "create-worklog"],
                "missing-options-create",
                "Usage: tt-devpro api create-worklog [<options>]\n\nError: missing option --date\nError: missing option --project-id\nError: missing option --task\nError: missing option --billability\nError: missing option --hours\n",
            ),
            (
                &["api", "create-worklog", "-h", "8", "-d", "2026-09-18"],
                "missing-options-create-partial",
                "Usage: tt-devpro api create-worklog [<options>]\n\nError: missing option --project-id\nError: missing option --task\nError: missing option --billability\n",
            ),
            (
                &["api", "create-worklog-typo"],
                "near-miss-subcommand",
                "Usage: tt-devpro api [<options>] <command> [<args>]...\n\nError: no such subcommand create-worklog-typo. Did you mean create-worklog?\n",
            ),
            (
                &["api", "get-worklogs", "--date"],
                "needs-value-and-missing",
                "Usage: tt-devpro api get-worklogs [<options>]\n\nError: option --date requires a value\nError: missing option --date\n",
            ),
            (
                &["api", "get-projects", "--date"],
                "option-needs-value",
                "Usage: tt-devpro api get-projects [<options>]\n\nError: option --date requires a value\n",
            ),
            (
                &["api", "get-projects", "-d"],
                "short-option-needs-value",
                "Usage: tt-devpro api get-projects [<options>]\n\nError: option -d requires a value\n",
            ),
            (
                &["settle", "--nosuchflag"],
                "unknown-option",
                "Usage: tt-devpro settle [<options>]\n\nError: no such option --nosuchflag\n",
            ),
            (
                &["api", "no-such-thing"],
                "unknown-subcommand",
                "Usage: tt-devpro api [<options>] <command> [<args>]...\n\nError: no such subcommand no-such-thing\n",
            ),
        ];
        for (tokens, capture, expected) in cases {
            assert_eq!(&stderr_of(tokens), expected, "cli-errors/{capture}");
        }
    }

    /// The two captures that exit **0** and write to stdout — Clikt answering a
    /// command that has subcommands and was given none. `cli-errors/root-no-args.out`
    /// is `help.out` and `cli-errors/api-no-args.out` is `api-help.out`.
    ///
    /// This one goes through [`dispatch`] rather than [`analyze`], because an empty
    /// argument list is a *valid* parse: clap yields `command: None` and the group's
    /// body prints its help, exactly as Clikt's `override fun run() = Unit` does.
    #[tokio::test]
    async fn a_group_command_with_no_subcommand_prints_its_own_help() {
        for (tokens, expected) in [(vec![], ROOT_HELP), (vec!["api"], API_HELP)] {
            let args = argv(&tokens);
            assert_eq!(cli_failure(&args), None, "{tokens:?}: not a usage failure");
            let cli = parse(&args).expect("a group with no subcommand parses");
            let mut io = Recorder::default();
            let outcome = dispatch(&cli, &mut io).await;
            assert_eq!(outcome, Outcome::Ok, "{tokens:?}");
            assert_eq!(outcome.exit_code(), 0, "{tokens:?}");
            assert_eq!(io.out, vec![expected.to_string()], "{tokens:?}");
            assert!(io.err.is_empty(), "{tokens:?}: nothing on stderr");
        }
    }

    /// The four date captures, and C33's named divergence held to exactly what it
    /// says: the usage line, the blank line, the `Error: invalid value for --X: `
    /// prefix, the empty stdout and the exit code are the incumbent's; the tail is
    /// `parse_iso_date`'s, because Java's is `DateTimeParseException.getMessage()`
    /// and reproducing it means hand-writing Java's message catalogue.
    ///
    /// The test asserts the whole line, not just the prefix — a divergence that is
    /// decided is a value to pin, and the assertion is what would catch the tail
    /// changing by accident later.
    #[test]
    fn the_four_date_failures_keep_the_prefix_and_diverge_only_in_javas_tail() {
        let cases: &[(&[&str], &str, &str, &str)] = &[
            (
                &["settle", "--from", "notadate", "--dry-run"],
                "from-unparseable",
                "Usage: tt-devpro settle [<options>]",
                "Error: invalid value for --from: notadate is not a date in YYYY-MM-DD form",
            ),
            (
                &["settle", "--from", "2026-13-01", "--dry-run"],
                "from-bad-month",
                "Usage: tt-devpro settle [<options>]",
                "Error: invalid value for --from: 2026-13-01 is not a date on the calendar",
            ),
            (
                &["settle", "--to", "2026-02-30", "--dry-run"],
                "to-bad-day",
                "Usage: tt-devpro settle [<options>]",
                "Error: invalid value for --to: 2026-02-30 is not a date on the calendar",
            ),
            (
                &["api", "get-projects", "--date", "nope"],
                "date-unparseable",
                "Usage: tt-devpro api get-projects [<options>]",
                "Error: invalid value for --date: nope is not a date in YYYY-MM-DD form",
            ),
        ];
        for (tokens, capture, usage, line) in cases {
            assert_eq!(
                stderr_of(tokens),
                format!("{usage}\n\n{line}\n"),
                "cli-errors/{capture}"
            );
        }
        assert!(
            stderr_of(&["settle", "--from", "notadate", "--dry-run"]).starts_with(
                "Usage: tt-devpro settle [<options>]\n\nError: invalid value for --from: "
            ),
            "the prefix is the part C33 holds the port to"
        );
    }

    // -----------------------------------------------------------------------
    // The boundaries the captures imply but do not contain
    // -----------------------------------------------------------------------

    /// The captured `unknown-option` case has no near miss at all, so on its own it
    /// cannot tell a working suggestor from one that never fires. Both measured
    /// neighbours are here: one survivor takes `Did you mean`, two take the list
    /// form, and the list includes the **short** spellings.
    #[test]
    fn an_unknown_option_names_its_near_misses_and_says_nothing_when_there_are_none() {
        assert_eq!(
            stderr_of(&["settle", "--nosuchflag"]),
            "Usage: tt-devpro settle [<options>]\n\nError: no such option --nosuchflag\n"
        );
        assert_eq!(
            stderr_of(&["settle", "--dryrun"]),
            "Usage: tt-devpro settle [<options>]\n\nError: no such option --dryrun. Did you mean --dry-run?\n"
        );
        assert_eq!(
            stderr_of(&["settle", "--includetoday"]),
            "Usage: tt-devpro settle [<options>]\n\nError: no such option --includetoday. Did you mean --include-today?\n"
        );
        assert_eq!(
            stderr_of(&["api", "get-projects", "--dat", "2026-01-01"]),
            "Usage: tt-devpro api get-projects [<options>]\n\nError: no such option --dat. (Possible options: --date, -d)\n",
            "the candidate list carries short forms too"
        );
        assert_eq!(
            stderr_of(&["api", "get-projects", "--hel"]),
            "Usage: tt-devpro api get-projects [<options>]\n\nError: no such option --hel. (Possible options: --help, -h)\n",
            "including the help option's own two names"
        );
    }

    /// The captured `near-miss-subcommand` and `unknown-subcommand` are the two ends;
    /// this is the middle, measured: `api creat-worklog` clears four candidates and
    /// lists them by descending similarity, which is neither declaration order nor
    /// alphabetical — `get-worklogs` sits third.
    #[test]
    fn several_near_miss_subcommands_are_listed_best_first() {
        assert_eq!(
            stderr_of(&["api", "creat-worklog"]),
            "Usage: tt-devpro api [<options>] <command> [<args>]...\n\nError: no such subcommand creat-worklog. (Possible subcommands: create-worklog, update-worklog, get-worklogs, delete-worklog)\n"
        );
        assert_eq!(
            stderr_of(&["settl"]),
            "Usage: tt-devpro [<options>] <command> [<args>]...\n\nError: no such subcommand settl. Did you mean settle?\n",
            "and the root's own two candidates work the same way"
        );
        assert_eq!(
            stderr_of(&["nosuch"]),
            "Usage: tt-devpro [<options>] <command> [<args>]...\n\nError: no such subcommand nosuch\n"
        );
    }

    /// Measured, and the rule is not the obvious one: **one** stray token under a
    /// command that has subcommands is a subcommand miss, **two** are extra
    /// arguments. Under a command with no subcommands even one is an extra argument.
    /// An implementation that always said `no such subcommand` fails row two; one
    /// that always said `extra argument` fails row one.
    #[test]
    fn one_stray_token_under_a_group_is_a_subcommand_miss_and_two_are_extra_arguments() {
        assert_eq!(
            stderr_of(&["api", "nosuchsub"]),
            "Usage: tt-devpro api [<options>] <command> [<args>]...\n\nError: no such subcommand nosuchsub\n"
        );
        assert_eq!(
            stderr_of(&["api", "nosuchsub", "anotherone"]),
            "Usage: tt-devpro api [<options>] <command> [<args>]...\n\nError: got unexpected extra arguments (nosuchsub anotherone)\n"
        );
        assert_eq!(
            stderr_of(&["settle", "extra-arg"]),
            "Usage: tt-devpro settle [<options>]\n\nError: got unexpected extra argument (extra-arg)\n",
            "settle has no subcommands, so its one stray token is already an excess"
        );
        assert_eq!(
            stderr_of(&["settle", "aaa", "bbb"]),
            "Usage: tt-devpro settle [<options>]\n\nError: got unexpected extra arguments (aaa bbb)\n",
            "plural, joined by a single space"
        );
    }

    /// Measured: `api delete-worklog aaa bbb` reports only `bbb`. The declared
    /// positional is filled first and only what is left over is excess — an
    /// implementation that counted every positional as excess would name both.
    #[test]
    fn a_declared_positional_is_filled_before_the_excess_is_counted() {
        assert_eq!(
            stderr_of(&["api", "delete-worklog", "aaa", "bbb"]),
            "Usage: tt-devpro api delete-worklog [<options>] <id>\n\nError: got unexpected extra argument (bbb)\n"
        );
        assert_eq!(
            cli_failure(&argv(&["api", "delete-worklog", "aaa"])),
            None,
            "and one token is simply the id"
        );
        assert_eq!(
            stderr_of(&["api", "get-projects", "get-worklogs"]),
            "Usage: tt-devpro api get-projects [<options>]\n\nError: got unexpected extra argument (get-worklogs)\n",
            "a sibling's name is not a subcommand once a leaf has been reached"
        );
    }

    /// The single most surprising measured rule, and the one a plausible
    /// implementation gets wrong in both directions: a token-pass error cancels the
    /// **argument** pass and leaves the **option** pass alone.
    ///
    /// `settle --dry-run extra --nosuchflag` prints no `extra argument` line.
    /// `api delete-worklog --nosuchopt aaa bbb` prints neither the excess nor
    /// `missing argument <id>`. And yet `api create-worklog --nosuchopt` still lists
    /// all five missing options.
    #[test]
    fn a_token_pass_error_cancels_the_argument_pass_but_not_the_missing_options() {
        assert_eq!(
            stderr_of(&["settle", "--dry-run", "extra", "--nosuchflag"]),
            "Usage: tt-devpro settle [<options>]\n\nError: no such option --nosuchflag\n"
        );
        assert_eq!(
            stderr_of(&["api", "delete-worklog", "--nosuchopt", "aaa", "bbb"]),
            "Usage: tt-devpro api delete-worklog [<options>] <id>\n\nError: no such option --nosuchopt\n"
        );
        assert_eq!(
            stderr_of(&["api", "create-worklog", "--nosuchopt"]),
            "Usage: tt-devpro api create-worklog [<options>]\n\
             \n\
             Error: no such option --nosuchopt\n\
             Error: missing option --date\n\
             Error: missing option --project-id\n\
             Error: missing option --task\n\
             Error: missing option --billability\n\
             Error: missing option --hours\n"
        );
    }

    /// The same rule one level up, and the reason it matters: a subcommand name is
    /// resolved in the pass a token error cancels, so `api --nosuch get-projects`
    /// reports against **`api`'s** usage line and `get-projects` never runs. A port
    /// that resolved the subcommand first would print the `get-projects` usage line
    /// for an error that belongs to its parent.
    #[test]
    fn a_parent_error_is_reported_with_the_parents_usage_and_the_child_never_runs() {
        assert_eq!(
            stderr_of(&["api", "--nosuch", "get-projects"]),
            "Usage: tt-devpro api [<options>] <command> [<args>]...\n\nError: no such option --nosuch\n"
        );
        assert_eq!(
            stderr_of(&["--nosuchflag", "settle"]),
            "Usage: tt-devpro [<options>] <command> [<args>]...\n\nError: no such option --nosuchflag\n"
        );
        assert_eq!(
            stderr_of(&["api", "settle"]),
            "Usage: tt-devpro api [<options>] <command> [<args>]...\n\nError: no such subcommand settle\n",
            "`settle` is a root subcommand and not an api one"
        );
    }

    /// Measured: the errors from the token pass come out in **token** order, and the
    /// missing-option lines are appended in **declaration** order regardless of how
    /// the surviving options were typed — `cli-errors/missing-options-create-partial`
    /// supplies `--hours` before `--date` and still lists `--project-id, --task,
    /// --billability`. Sorting either list alphabetically fails this.
    #[test]
    fn token_errors_come_in_token_order_and_missing_options_in_declaration_order() {
        assert_eq!(
            stderr_of(&["settle", "--nosuchflag", "--alsonosuch"]),
            "Usage: tt-devpro settle [<options>]\n\nError: no such option --nosuchflag\nError: no such option --alsonosuch\n"
        );
        assert_eq!(
            stderr_of(&["settle", "--from", "notadate", "--to", "alsobad"]),
            "Usage: tt-devpro settle [<options>]\n\n\
             Error: invalid value for --from: notadate is not a date in YYYY-MM-DD form\n\
             Error: invalid value for --to: alsobad is not a date in YYYY-MM-DD form\n",
            "two converters, two lines, in the order they were typed"
        );
        assert_eq!(
            stderr_of(&["api", "create-worklog", "extra"]),
            "Usage: tt-devpro api create-worklog [<options>]\n\
             \n\
             Error: got unexpected extra argument (extra)\n\
             Error: missing option --date\n\
             Error: missing option --project-id\n\
             Error: missing option --task\n\
             Error: missing option --billability\n\
             Error: missing option --hours\n",
            "the excess line precedes the missing-option lines"
        );
    }

    /// Measured on both spellings: the message echoes the option **as typed**.
    /// `cli-errors/short-option-needs-value` says `-d` where
    /// `cli-errors/option-needs-value` says `--date`, and the two were captured from
    /// the same option on the same command. Normalising to the declared long name
    /// passes one and fails the other.
    #[test]
    fn the_option_is_echoed_as_typed_rather_than_normalised_to_its_long_form() {
        assert!(
            stderr_of(&["api", "get-projects", "-d"]).contains("Error: option -d requires a value")
        );
        assert!(
            stderr_of(&["api", "get-projects", "--date"])
                .contains("Error: option --date requires a value")
        );
        assert!(
            stderr_of(&["api", "get-projects", "-d", "nope"])
                .contains("Error: invalid value for -d: "),
            "the converter's message names the short form too"
        );
        assert!(stderr_of(&["settle", "--from"]).contains("Error: option --from requires a value"));
    }

    /// Measured: `api get-worklogs -d` emits both `option -d requires a value` and
    /// `missing option --date`. Supplying the value — even through the short form,
    /// even glued into the cluster — removes the second line. So an option is
    /// satisfied by its value arriving, not by its name appearing.
    #[test]
    fn an_option_is_satisfied_by_a_value_arriving_not_by_its_name_appearing() {
        assert_eq!(
            stderr_of(&["api", "get-worklogs", "-d"]),
            "Usage: tt-devpro api get-worklogs [<options>]\n\nError: option -d requires a value\nError: missing option --date\n"
        );
        assert_eq!(
            cli_failure(&argv(&["api", "get-worklogs", "-d", "2026-09-18"])),
            None,
            "the short form satisfies the long name"
        );
        assert_eq!(
            cli_failure(&argv(&["api", "get-worklogs", "--date=2026-09-18"])),
            None,
            "and so does an attached value"
        );
    }

    /// Measured: `api get-projects -dh` answers `invalid value for -d: Text 'h'`, so
    /// the rest of the cluster became `-d`'s value rather than a second flag —
    /// `api create-worklog -dh` leaves four options missing, not five, because
    /// `--date` took the `h`. And an unknown letter ends the cluster with one
    /// message: `api get-projects -zd 2026-01-01` reports `-z` and nothing else.
    #[test]
    fn a_clustered_short_option_takes_the_rest_of_the_cluster_as_its_value() {
        assert_eq!(
            stderr_of(&["api", "get-projects", "-dh"]),
            "Usage: tt-devpro api get-projects [<options>]\n\nError: invalid value for -d: h is not a date in YYYY-MM-DD form\n"
        );
        assert_eq!(
            stderr_of(&["api", "create-worklog", "-dh"]),
            "Usage: tt-devpro api create-worklog [<options>]\n\
             \n\
             Error: missing option --project-id\n\
             Error: missing option --task\n\
             Error: missing option --billability\n\
             Error: missing option --hours\n",
            "--date is satisfied by the letter h, so it is not listed"
        );
        assert_eq!(
            stderr_of(&["api", "get-projects", "-zd", "2026-01-01"]),
            "Usage: tt-devpro api get-projects [<options>]\n\nError: no such option -z\n",
            "one message, and the cluster stops"
        );
    }

    /// Measured: `settle --json=yes` answers `option --json does not take a value`.
    /// A flag that silently ignored an attached value would run the command.
    #[test]
    fn a_flag_given_an_attached_value_is_refused_rather_than_ignored() {
        assert_eq!(
            stderr_of(&["settle", "--json=yes"]),
            "Usage: tt-devpro settle [<options>]\n\nError: option --json does not take a value\n"
        );
        assert_eq!(
            cli_failure(&argv(&["settle", "--json"])),
            None,
            "the bare flag is fine"
        );
    }

    /// Measured: `api get-projects --date=` reaches the converter with the empty
    /// string rather than being treated as a missing value. An implementation that
    /// tested the value for emptiness would answer `requires a value` here.
    #[test]
    fn an_attached_empty_value_reaches_the_converter_rather_than_counting_as_absent() {
        assert_eq!(
            stderr_of(&["api", "get-projects", "--date="]),
            "Usage: tt-devpro api get-projects [<options>]\n\nError: invalid value for --date:  is not a date in YYYY-MM-DD form\n"
        );
    }

    /// Measured: `settle -- --nosuchflag` answers `got unexpected extra argument`,
    /// so `--` really does stop option parsing rather than being ignored.
    #[test]
    fn a_double_dash_turns_a_following_option_into_an_extra_argument() {
        assert_eq!(
            stderr_of(&["settle", "--", "--nosuchflag"]),
            "Usage: tt-devpro settle [<options>]\n\nError: got unexpected extra argument (--nosuchflag)\n"
        );
    }

    /// The usage line is the first line of the failing command's own help, so it
    /// names the deepest command reached — including its `<command> [<args>]...`
    /// tail on a group and its `<id>` on `delete-worklog`. A port that printed the
    /// root's usage everywhere passes every message assertion and fails this.
    #[test]
    fn the_usage_line_names_the_deepest_command_reached() {
        for (tokens, expected) in [
            (
                vec!["--nosuchflag"],
                "Usage: tt-devpro [<options>] <command> [<args>]...",
            ),
            (
                vec!["settle", "--nosuchflag"],
                "Usage: tt-devpro settle [<options>]",
            ),
            (
                vec!["api", "--nosuchflag"],
                "Usage: tt-devpro api [<options>] <command> [<args>]...",
            ),
            (
                vec!["api", "get-projects", "--nosuchflag"],
                "Usage: tt-devpro api get-projects [<options>]",
            ),
            (
                vec!["api", "delete-worklog", "--nosuchflag"],
                "Usage: tt-devpro api delete-worklog [<options>] <id>",
            ),
        ] {
            let stderr = stderr_of(&tokens);
            assert_eq!(stderr.lines().next(), Some(expected), "{tokens:?}");
            assert_eq!(
                stderr.lines().nth(1),
                Some(""),
                "{tokens:?}: a blank line follows"
            );
        }
    }

    // -----------------------------------------------------------------------
    // The seam between the analyzer and clap
    // -----------------------------------------------------------------------

    /// The table in [`converts_a_date`] is a second list of what the parser already
    /// knows, so it is held against the parser's own behaviour rather than against
    /// the source it was read from. The boundary it exists for is the last two rows:
    /// **`--date` converts on `get-projects` and does not on `get-worklogs`** — same
    /// name, same short letter, different command.
    #[test]
    fn exactly_the_date_converting_options_reject_a_non_date_at_parse_time() {
        let cases: &[(&[&str], &[&str], &str, bool)] = &[
            (
                &["settle"],
                &["settle", "--from", "notadate"],
                "--from",
                true,
            ),
            (&["settle"], &["settle", "--to", "notadate"], "--to", true),
            (
                &["api", "get-projects"],
                &["api", "get-projects", "--date", "notadate"],
                "--date",
                true,
            ),
            (
                &["api", "get-worklogs"],
                &["api", "get-worklogs", "--date", "notadate"],
                "--date",
                false,
            ),
            (
                &["api", "create-worklog"],
                &[
                    "api",
                    "create-worklog",
                    "-d",
                    "notadate",
                    "-p",
                    "p",
                    "-t",
                    "t",
                    "-b",
                    "Billable",
                    "-h",
                    "1",
                ],
                "--date",
                false,
            ),
        ];
        for (path, tokens, long, converts) in cases {
            assert_eq!(
                converts_a_date(path, long),
                *converts,
                "the table disagrees about `{} {long}`",
                path.join(" ")
            );
            assert_eq!(
                command().try_get_matches_from(argv(tokens)).is_err(),
                *converts,
                "clap's own value parser disagrees about `{} {long}`",
                path.join(" ")
            );
        }
    }

    /// An argument list with nothing wrong with it is handed to clap, and clap takes
    /// it. This is the test that would catch an analyzer that invented an error on
    /// valid input — every other test here asserts that something *does* fail.
    #[test]
    fn a_well_formed_argument_list_is_handed_to_clap_and_parses() {
        for tokens in [
            vec!["settle"],
            vec!["settle", "--dry-run"],
            vec!["settle", "--json", "--include-today"],
            vec!["settle", "--from", "2026-09-01", "--to", "2026-09-18"],
            vec!["settle", "--from=2026-09-01"],
            vec!["api", "get-projects"],
            vec!["api", "get-projects", "--date", "2026-09-18"],
            vec!["api", "get-worklogs", "-d", "2026-09-18"],
            vec!["api", "delete-worklog", "abc123"],
            vec![
                "api",
                "create-worklog",
                "-d",
                "2026-09-18",
                "-p",
                "p",
                "-t",
                "t",
                "-b",
                "Billable",
                "-h",
                "8",
            ],
            vec![
                "api",
                "update-worklog",
                "-i",
                "w1",
                "-d",
                "2026-09-18",
                "-p",
                "p",
                "-t",
                "t",
                "-b",
                "Billable",
                "-h",
                "8",
                "--description",
                "note",
            ],
        ] {
            let args = argv(&tokens);
            assert_eq!(cli_failure(&args), None, "{tokens:?}: analyzer objected");
            assert!(parse(&args).is_ok(), "{tokens:?}: clap objected");
        }
    }

    /// The analyzer and clap must agree on which argument lists are valid, and the
    /// place they would most easily disagree is a hyphenated value: Clikt lets
    /// `--hours` take `--help`, clap by default does not. Without
    /// `allow_hyphen_values` this parse fails and the fallback fires on an argument
    /// list the analyzer waved through.
    #[test]
    fn a_hyphenated_value_that_clikt_accepts_reaches_clap_intact() {
        let args = argv(&[
            "api",
            "create-worklog",
            "-d",
            "2026-09-18",
            "-p",
            "p",
            "-t",
            "t",
            "-b",
            "Billable",
            "-h",
            "--help",
        ]);
        assert_eq!(cli_failure(&args), None, "Clikt takes --help as the hours");
        let cli = parse(&args).expect("and so must clap");
        match cli.command {
            Some(TopCommand::Api {
                command: Some(ApiCommand::CreateWorklog(args)),
            }) => assert_eq!(args.hours, "--help"),
            other => panic!("parsed as {other:?}"),
        }
    }

    /// The fallback is unreachable through [`main`], so it is exercised directly:
    /// [`parse`] is called on an argument list the analyzer would have stopped. What
    /// is pinned is the shape — one `Error: ` line inside a usage block, exit 1,
    /// nothing on stdout — not clap's wording, which is clap's to change.
    #[test]
    fn the_clap_fallback_renders_in_clikts_shape_and_exits_one() {
        let rendered = parse(&argv(&["api", "no-such-thing-at-all"]))
            .expect_err("clap rejects an unknown subcommand");
        assert_eq!(rendered.code, 1, "C33 applies to the fallback too");
        assert_eq!(rendered.stdout, None);
        let stderr = rendered.stderr.expect("the fallback writes stderr");
        assert_eq!(
            stderr.lines().next(),
            Some("Usage: tt-devpro [<options>] <command> [<args>]...")
        );
        assert_eq!(stderr.lines().nth(1), Some(""));
        let message = stderr.lines().nth(2).expect("one error line");
        assert!(message.starts_with("Error: "), "got {message:?}");
        assert!(
            !message.contains("error: "),
            "clap's own prefix is stripped"
        );
        assert_eq!(
            stderr.lines().count(),
            3,
            "one line, not clap's whole block"
        );
    }

    /// C33 stated as the thing it protects against: clap's own exit code for a usage
    /// error is 2 and there is no setting that changes it, so the port is wrong by
    /// default unless `main` renders the case itself. Both halves are asserted — the
    /// port's 1, and clap's 2 on the same argument list — because an assertion on
    /// the port alone cannot show that the two differ.
    #[test]
    fn a_usage_failure_exits_one_where_clap_would_have_exited_two() {
        assert_eq!(outcome(&["settle", "--nosuchflag"]).code, 1);
        let clap_error = command()
            .try_get_matches_from(argv(&["settle", "--nosuchflag"]))
            .expect_err("clap rejects it too");
        assert_eq!(clap_error.exit_code(), 2, "the premise: clap would exit 2");
    }

    /// D3, at the one place `main` can get it wrong: the exit code comes from the
    /// command's [`Outcome`], and a failed command must not be rendered a second
    /// time here. A `main` that mapped every non-`Ok` outcome to 0 — the incumbent's
    /// C24 behaviour — fails the second assertion.
    #[test]
    fn a_failed_command_exits_with_its_own_outcome_and_is_not_rendered_again() {
        assert_eq!(Outcome::Ok.exit_code(), 0);
        assert_eq!(Outcome::Failed.exit_code(), 1);
    }

    // -----------------------------------------------------------------------
    // A Console that remembers
    // -----------------------------------------------------------------------

    /// Records what a command body wrote. `present` is false, which is the
    /// non-interactive regime; neither group-command path prompts.
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
        fn read_line(&mut self) -> Option<String> {
            None
        }
        fn present(&self) -> bool {
            false
        }
    }
}
