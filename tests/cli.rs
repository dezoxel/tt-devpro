//! End-to-end tests that run the **actual `tt-devpro` binary** and hold its three
//! streams against the captured behaviour of the Kotlin incumbent.
//!
//! Every other layer of this port is unit-tested in process. This file exists for
//! the one thing a unit test cannot say: that the process a caller invokes prints
//! those bytes, on that stream, and exits with that code.
//!
//! # The oracle
//!
//! `~/.cache/tt-devpro-rewrite/baseline/`, pinned by `MANIFEST.sha256`. Each case is
//! three files — `NAME.out`, `NAME.err`, `NAME.code` — captured from
//! `tt-devpro.kotlin-incumbent` (sha256 `c2aef117…4ce35c`, the pre-rewrite binary
//! kept as the rollback). The argv for each case comes from that directory's
//! `README.md` and from the harness case list at
//! `~/.cache/tt-devpro-rewrite/run-notes/parity/self-run-3.log`, **not** from
//! reading the case name and guessing.
//!
//! Only the **help surface and the argument-parsing failures** are a durable
//! oracle. The live captures in the same directory (`settle-dryrun`, `settle-json`,
//! `api-get-projects`, `api-get-worklogs-0918`, `settle-range-*`) read Chrono and
//! the portal, both of which change daily; they belong to the step-6 differential
//! harness at `~/.cache/tt-devpro-rewrite/parity/`, not here.
//!
//! # Compare bytes
//!
//! Every assertion here is equality on the whole stream, trailing newline included.
//! A `contains` assertion passes against an implementation that prints the right
//! line and then garbage, which is exactly the failure a byte-compared CLI is
//! supposed to make impossible.
//!
//! The one exception is the four C33 date cases, where the port deliberately
//! diverges in the message tail. Those go through
//! `the_incumbents_prefix_and_our_own_tail`, which pins **both** tails and asserts
//! that they still differ — so changing either one fails a test that names the
//! recorded decision rather than drifting quietly.
//!
//! # The safety rule
//!
//! A real `settle` POSTs worklogs to the live Dev.Pro portal, which other people
//! read. **No invocation in this file can reach the network.** Every argv here
//! either prints help or fails in the parser, before any client is built.
//!
//! There is no bare `settle`, no `settle --dry-run`, no `settle --json`, no
//! `api get-projects` with a valid date, and no `api create-worklog` /
//! `update-worklog` / `delete-worklog` in a form that parses. A bogus cookie is
//! **not** a substitute: `portal::BASE_URL` is the live host, and a request that
//! fails authentication is still a request to production.
//!
//! `bin` adds a second, independent guard on top of that reasoning — see its
//! doc comment. It is a backstop, not the argument: the argument is that every argv
//! below fails in the parser.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::OnceLock;

use assert_cmd::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture plumbing
// ---------------------------------------------------------------------------

/// A pointed-at-nothing `HOME`, shared by every invocation in this file.
///
/// C30: `dirs::home_dir()` reads `$HOME` first on Unix, where the JVM's
/// `user.home` ignores it and goes to the passwd entry. The port keeps
/// `dirs::home_dir()` precisely because that makes it testable, and this is the
/// test that spends it: with `HOME` pointing at an empty directory,
/// `~/.config/tt-devpro/config.yaml` is absent and with it the cookie's 1Password reference, so a
/// command body that somehow ran would die in `config::load` or
/// `cookie::session_cookie` — both of which bail before any client is constructed.
fn guard_home() -> &'static Path {
    static GUARD: OnceLock<TempDir> = OnceLock::new();
    GUARD
        .get_or_init(|| tempfile::tempdir().expect("a temporary directory for the HOME guard"))
        .path()
}

/// The binary under test, with the guard applied.
///
/// `TT_COOKIE` is removed rather than set: C20 takes it before anything else, so
/// leaving an inherited `TT_COOKIE` in place would hand a command body a usable live
/// session.
fn bin() -> Command {
    let mut command = Command::cargo_bin("tt-devpro").expect("the tt-devpro binary should build");
    command.env("HOME", guard_home()).env_remove("TT_COOKIE");
    command
}

fn run(argv: &[&str]) -> Output {
    bin()
        .args(argv)
        .output()
        .expect("the tt-devpro binary should run")
}

/// The captured behaviour of one incumbent invocation: all three streams at once.
struct Capture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
}

fn baseline_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME is set for the test runner"))
        .join(".cache/tt-devpro-rewrite/baseline")
}

fn help_captures() -> PathBuf {
    baseline_dir()
}

fn error_captures() -> PathBuf {
    baseline_dir().join("cli-errors")
}

fn read_capture_file(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| {
        panic!(
            "missing capture {}: {error}\n\
             The oracle lives at ~/.cache/tt-devpro-rewrite/baseline/ and is pinned by its \
             MANIFEST.sha256. Regenerate it from ~/.cache/tt-devpro-rewrite/tt-devpro.kotlin-incumbent \
             as that directory's README.md describes — never with a bare `settle`.",
            path.display()
        )
    })
}

/// Read one three-file case.
///
/// **The exit code is compared as a parsed integer, not as bytes.** Three of the
/// `.code` files — `help`, `api-help`, `settle-help` — hold `0 ` with a trailing
/// space, an artifact of how those three were captured rather than anything the
/// binary does. `baseline/README.md` records it and says to compare the integer.
fn capture(dir: &Path, name: &str) -> Capture {
    let code = String::from_utf8(read_capture_file(&dir.join(format!("{name}.code"))))
        .expect("an exit-code capture is ASCII");
    Capture {
        stdout: read_capture_file(&dir.join(format!("{name}.out"))),
        stderr: read_capture_file(&dir.join(format!("{name}.err"))),
        code: code.trim().parse().unwrap_or_else(|error| {
            panic!("exit code capture {name}.code is not a number: {error}")
        }),
    }
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Hold all three streams of a live run against all three capture files.
///
/// This is the whole fixture: a test is its doc comment, its name, and one call.
#[track_caller]
fn identical_to_capture(dir: &Path, name: &str, argv: &[&str]) {
    let want = capture(dir, name);
    let got = run(argv);
    assert_eq!(
        show(&got.stdout),
        show(&want.stdout),
        "stdout differs from capture {name} for argv {argv:?}"
    );
    assert_eq!(
        show(&got.stderr),
        show(&want.stderr),
        "stderr differs from capture {name} for argv {argv:?}"
    );
    assert_eq!(
        got.status.code(),
        Some(want.code),
        "exit code differs from capture {name} for argv {argv:?}"
    );
}

#[track_caller]
fn matches_help_capture(name: &str, argv: &[&str]) {
    identical_to_capture(&help_captures(), name, argv);
}

#[track_caller]
fn matches_error_capture(name: &str, argv: &[&str]) {
    identical_to_capture(&error_captures(), name, argv);
}

/// C33's four date cases: identical everywhere except the message tail.
///
/// Asserts, in order: stdout is empty and equals the capture's; the exit code is
/// the capture's and is 1; the capture's stderr is exactly
/// `<head><java_tail>\n`; the port's stderr is exactly `<head><port_tail>\n`,
/// where `head` is the capture's own bytes up to and including `prefix`; and that
/// the two tails still differ.
///
/// The last assertion is the point of the helper. C33 records this divergence as a
/// decision — Java's tail is `DateTimeParseException.getMessage()` and reproducing
/// it means hand-writing Java's message catalogue for a string no caller parses.
/// If someone ever makes the two agree, that is a *change to a recorded decision*
/// and this test should say so rather than quietly going green.
#[track_caller]
fn the_incumbents_prefix_and_our_own_tail(
    dir: &Path,
    name: &str,
    argv: &[&str],
    prefix: &str,
    java_tail: &str,
    port_tail: &str,
) {
    let want = capture(dir, name);
    let got = run(argv);

    assert_eq!(
        show(&got.stdout),
        show(&want.stdout),
        "C33 holds stdout identical for {name}"
    );
    assert_eq!(
        show(&got.stdout),
        "",
        "C33: a usage failure writes no stdout"
    );
    assert_eq!(
        got.status.code(),
        Some(want.code),
        "C33 holds the exit code identical for {name}"
    );
    assert_eq!(want.code, 1, "C33: the incumbent's usage failures exit 1");

    let want_err = show(&want.stderr);
    let got_err = show(&got.stderr);
    let cut = want_err
        .find(prefix)
        .unwrap_or_else(|| panic!("capture {name} does not carry the prefix {prefix:?}"))
        + prefix.len();
    let head = &want_err[..cut];

    assert_eq!(
        want_err,
        format!("{head}{java_tail}\n"),
        "the pinned Java tail of {name} has changed"
    );
    assert_eq!(
        got_err,
        format!("{head}{port_tail}\n"),
        "the port's tail for {name} has changed; C33 records it as a deliberate divergence"
    );
    assert_ne!(
        java_tail, port_tail,
        "C33 records these tails as divergent; if they now agree, the contract note is stale"
    );
}

#[track_caller]
fn diverging_date_error(name: &str, argv: &[&str], prefix: &str, java_tail: &str, port_tail: &str) {
    the_incumbents_prefix_and_our_own_tail(
        &error_captures(),
        name,
        argv,
        prefix,
        java_tail,
        port_tail,
    );
}

// The usage line of each command, which every failure under it reprints. Each one
// is also the first line of that command's help capture — held there by
// `every_usage_line_constant_is_the_first_line_of_its_own_help_capture`, so these
// are transcriptions under test rather than eight chances to typo.
const USAGE_ROOT: &str = "Usage: tt-devpro [<options>] <command> [<args>]...";
const USAGE_SETTLE: &str = "Usage: tt-devpro settle [<options>]";
const USAGE_API: &str = "Usage: tt-devpro api [<options>] <command> [<args>]...";
const USAGE_GET_PROJECTS: &str = "Usage: tt-devpro api get-projects [<options>]";
const USAGE_GET_WORKLOGS: &str = "Usage: tt-devpro api get-worklogs [<options>]";
const USAGE_CREATE_WORKLOG: &str = "Usage: tt-devpro api create-worklog [<options>]";
const USAGE_UPDATE_WORKLOG: &str = "Usage: tt-devpro api update-worklog [<options>]";
const USAGE_DELETE_WORKLOG: &str = "Usage: tt-devpro api delete-worklog [<options>] <id>";

/// The usage block as the incumbent lays it out: the usage line, a blank line, then
/// one `Error: ` line per problem, in the order they were found.
///
/// Used only for the boundaries that have no capture file of their own; the 25
/// captured cases are compared against their files and never against this.
fn usage_block(usage: &str, errors: &[&str]) -> String {
    let lines: Vec<String> = errors
        .iter()
        .map(|error| format!("Error: {error}"))
        .collect();
    format!("{usage}\n\n{}\n", lines.join("\n"))
}

/// An invocation that fails in the parser: empty stdout, the usage block on stderr,
/// exit 1 (C33 — clap's own default is 2 and has no setting that changes it).
#[track_caller]
fn fails_with(argv: &[&str], usage: &str, errors: &[&str]) {
    let got = run(argv);
    assert_eq!(show(&got.stdout), "", "a usage failure writes no stdout");
    assert_eq!(
        show(&got.stderr),
        usage_block(usage, errors),
        "argv {argv:?}"
    );
    assert_eq!(got.status.code(), Some(1), "C33: a usage failure exits 1");
}

/// An invocation that prints some command's help: that capture's stdout verbatim,
/// nothing on stderr, exit 0.
///
/// The expected text is the captured help of the named command, so a help-routing
/// test cannot pass by printing the *wrong* command's help.
#[track_caller]
fn prints_the_help_of(argv: &[&str], capture_name: &str) {
    let want = capture(&help_captures(), capture_name);
    let got = run(argv);
    assert_eq!(
        show(&got.stdout),
        show(&want.stdout),
        "argv {argv:?} should print the help captured as {capture_name}"
    );
    assert_eq!(show(&got.stderr), "", "help goes to stdout, never stderr");
    assert_eq!(
        got.status.code(),
        Some(0),
        "Clikt's PrintHelpMessage(error = false) exits 0"
    );
}

// ---------------------------------------------------------------------------
// The eight help captures — D6, the help surface served as captured bytes
// ---------------------------------------------------------------------------

/// D6 — `baseline/help.out`. Catches a port that lets clap render its own help,
/// which differs from Clikt's in the usage line, the option column and the order.
#[test]
fn root_help_is_the_incumbents_own_bytes() {
    matches_help_capture("help", &["--help"]);
}

/// D6 — `baseline/settle-help.out`. The longest of the seven, with wrapped option
/// descriptions; catches a reflowed or re-indented help column.
#[test]
fn settle_help_is_the_incumbents_own_bytes() {
    matches_help_capture("settle-help", &["settle", "--help"]);
}

/// D6 — `baseline/api-help.out`. The group listing, in registration order rather
/// than alphabetical order (`ApiCommand.kt:221-227`).
#[test]
fn api_group_help_is_the_incumbents_own_bytes() {
    matches_help_capture("api-help", &["api", "--help"]);
}

/// D6, C8 — `baseline/api-get-projects-help.out`. Carries `-d, --date`, the short
/// form the C33 short-spelling cases depend on.
#[test]
fn api_get_projects_help_is_the_incumbents_own_bytes() {
    matches_help_capture("api-get-projects-help", &["api", "get-projects", "--help"]);
}

/// D6 — `baseline/api-get-worklogs-help.out`. Its `--date` metavar differs from
/// get-projects', because this one does not convert (`ApiCommand.kt:74`).
#[test]
fn api_get_worklogs_help_is_the_incumbents_own_bytes() {
    matches_help_capture("api-get-worklogs-help", &["api", "get-worklogs", "--help"]);
}

/// D6, C21 — `baseline/api-create-worklog-help.out`. This capture is where C21 is
/// visible in the help text itself: the line reads `--help` alone, with no `-h, `,
/// because `-h` is `--hours` here.
#[test]
fn api_create_worklog_help_offers_only_the_long_help_because_short_h_is_hours() {
    matches_help_capture(
        "api-create-worklog-help",
        &["api", "create-worklog", "--help"],
    );
}

/// D6, C21 — `baseline/api-update-worklog-help.out`. The second of the two write
/// subcommands that take `-h` for `--hours`.
#[test]
fn api_update_worklog_help_offers_only_the_long_help_because_short_h_is_hours() {
    matches_help_capture(
        "api-update-worklog-help",
        &["api", "update-worklog", "--help"],
    );
}

/// D6 — `baseline/api-delete-worklog-help.out`. The only command in the tree with a
/// positional, and its usage line carries `<id>`.
#[test]
fn api_delete_worklog_help_is_the_incumbents_own_bytes() {
    matches_help_capture(
        "api-delete-worklog-help",
        &["api", "delete-worklog", "--help"],
    );
}

/// The eight `USAGE_*` constants below are transcriptions, and a transcription can
/// be wrong. Each one is the first line of the help capture for the same command,
/// so this holds them against the oracle instead of against a careful read.
#[test]
fn every_usage_line_constant_is_the_first_line_of_its_own_help_capture() {
    let pairs = [
        (USAGE_ROOT, "help"),
        (USAGE_SETTLE, "settle-help"),
        (USAGE_API, "api-help"),
        (USAGE_GET_PROJECTS, "api-get-projects-help"),
        (USAGE_GET_WORKLOGS, "api-get-worklogs-help"),
        (USAGE_CREATE_WORKLOG, "api-create-worklog-help"),
        (USAGE_UPDATE_WORKLOG, "api-update-worklog-help"),
        (USAGE_DELETE_WORKLOG, "api-delete-worklog-help"),
    ];
    for (usage, capture_name) in pairs {
        let help = show(&capture(&help_captures(), capture_name).stdout);
        assert_eq!(
            help.lines().next(),
            Some(usage),
            "the usage constant for {capture_name} is not that capture's first line"
        );
    }
}

// ---------------------------------------------------------------------------
// The seventeen cli-errors captures — C33, the argument-parsing failure surface
// ---------------------------------------------------------------------------

/// `cli-errors/root-no-args` — argv `` (empty).
///
/// A group command reached with no subcommand prints its own help on **stdout**
/// and exits **0**: Clikt's `override fun run() = Unit` (`Main.kt:16`). Two of the
/// seventeen error captures are not errors at all, and this is one; a port that
/// made the subcommand required would turn it into one.
#[test]
fn no_arguments_at_all_prints_the_root_help_on_stdout_and_exits_zero() {
    matches_error_capture("root-no-args", &[]);
}

/// `cli-errors/api-no-args` — argv `api`. The same rule one level down
/// (`ApiCommand.kt:26`), and it prints the **api** group's help, not the root's.
#[test]
fn the_api_group_with_no_subcommand_prints_its_own_help_and_exits_zero() {
    matches_error_capture("api-no-args", &["api"]);
}

/// `cli-errors/unknown-option` — argv `settle --nosuchflag`.
///
/// `--nosuchflag` scores no near miss, so the message carries no suggestion list.
/// Pairs with `a_near_miss_long_option_lists_both_its_spellings`, which does.
#[test]
fn an_unknown_long_option_is_reported_against_its_own_commands_usage_line() {
    matches_error_capture("unknown-option", &["settle", "--nosuchflag"]);
}

/// `cli-errors/unknown-subcommand` — argv `api no-such-thing`.
///
/// One excess token under a command that *has* subcommands is a subcommand miss,
/// not an extra argument. Contrast
/// `two_excess_tokens_under_a_group_are_extra_arguments_not_a_subcommand_miss`.
#[test]
fn a_single_unrecognised_token_under_a_group_is_reported_as_a_missing_subcommand() {
    matches_error_capture("unknown-subcommand", &["api", "no-such-thing"]);
}

/// `cli-errors/near-miss-subcommand` — argv `api create-worklog-typo`.
///
/// The suggestion surface: `Did you mean create-worklog?`. Catches a port that
/// reports the miss correctly and drops the suggestion, which is the half a
/// `contains`-style assertion would not notice.
#[test]
fn a_near_miss_subcommand_suggests_the_one_it_nearly_matched() {
    matches_error_capture("near-miss-subcommand", &["api", "create-worklog-typo"]);
}

/// `cli-errors/from-unparseable` — argv `settle --from notadate --dry-run`.
///
/// C33's named divergence. Everything but the tail is byte-identical: the usage
/// line, the blank line, the empty stdout, the exit code 1 and the
/// `Error: invalid value for --from: ` prefix.
///
/// Note what this case also proves: `--dry-run` is on the command line and nothing
/// ran. The failure is in the parser, which is why this argv is safe here.
#[test]
fn an_unparseable_from_date_keeps_the_incumbents_prefix_and_carries_chronos_tail() {
    diverging_date_error(
        "from-unparseable",
        &["settle", "--from", "notadate", "--dry-run"],
        "Error: invalid value for --from: ",
        "Text 'notadate' could not be parsed at index 0",
        "notadate is not a date in YYYY-MM-DD form",
    );
}

/// `cli-errors/from-bad-month` — argv `settle --from 2026-13-01 --dry-run`.
///
/// C33, and the second of the two port tails: a well-shaped string whose fields are
/// out of range says `is not a date on the calendar`, where an unparseable one says
/// `is not a date in YYYY-MM-DD form`. Both spellings are live, and a port that
/// collapsed them to one message would fail exactly one of these two tests.
#[test]
fn a_from_date_with_month_thirteen_reports_a_calendar_failure_not_a_shape_failure() {
    diverging_date_error(
        "from-bad-month",
        &["settle", "--from", "2026-13-01", "--dry-run"],
        "Error: invalid value for --from: ",
        "Text '2026-13-01' could not be parsed: Invalid value for MonthOfYear (valid values 1 - 12): 13",
        "2026-13-01 is not a date on the calendar",
    );
}

/// `cli-errors/to-bad-day` — argv `settle --to 2026-02-30 --dry-run`.
///
/// C33, and the only capture that exercises `--to` rather than `--from`, so it is
/// what stops the two options sharing one hard-coded name in the message.
#[test]
fn a_to_date_of_february_thirtieth_names_to_rather_than_from() {
    diverging_date_error(
        "to-bad-day",
        &["settle", "--to", "2026-02-30", "--dry-run"],
        "Error: invalid value for --to: ",
        "Text '2026-02-30' could not be parsed: Invalid date 'FEBRUARY 30'",
        "2026-02-30 is not a date on the calendar",
    );
}

/// `cli-errors/date-unparseable` — argv `api get-projects --date nope`.
///
/// C33 on the third and last converting option (`ApiCommand.kt:53`). Its sibling
/// `api get-worklogs --date` does **not** convert, so a port that validated every
/// option called `--date` would pass this and change `get-worklogs`.
#[test]
fn an_unparseable_get_projects_date_fails_in_the_parser_with_its_own_usage_line() {
    diverging_date_error(
        "date-unparseable",
        &["api", "get-projects", "--date", "nope"],
        "Error: invalid value for --date: ",
        "Text 'nope' could not be parsed at index 0",
        "nope is not a date in YYYY-MM-DD form",
    );
}

/// `cli-errors/option-needs-value` — argv `api get-projects --date`.
///
/// The option is echoed **as the caller typed it**. Pairs with
/// `a_short_option_left_without_a_value_is_echoed_in_its_short_spelling`: a port
/// that normalised to the declared long name passes one and fails the other.
#[test]
fn a_long_option_left_without_a_value_is_echoed_in_its_long_spelling() {
    matches_error_capture("option-needs-value", &["api", "get-projects", "--date"]);
}

/// `cli-errors/short-option-needs-value` — argv `api get-projects -d`.
///
/// `Error: option -d requires a value`, not `--date`. The other half of the pair
/// above.
#[test]
fn a_short_option_left_without_a_value_is_echoed_in_its_short_spelling() {
    matches_error_capture("short-option-needs-value", &["api", "get-projects", "-d"]);
}

/// `cli-errors/needs-value-and-missing` — argv `api get-worklogs --date`.
///
/// A `requires a value` error does not satisfy the option: both
/// `option --date requires a value` and `missing option --date` come out, in that
/// order. Catches a port that marks an option satisfied on sight of its name.
#[test]
fn an_option_named_but_left_valueless_is_both_valueless_and_still_missing() {
    matches_error_capture(
        "needs-value-and-missing",
        &["api", "get-worklogs", "--date"],
    );
}

/// `cli-errors/missing-option-worklogs` — argv `api get-worklogs`.
///
/// The one-required-option case, and the shortest proof that `get-worklogs` never
/// reaches the portal without a date.
#[test]
fn get_worklogs_without_its_required_date_fails_before_any_request() {
    matches_error_capture("missing-option-worklogs", &["api", "get-worklogs"]);
}

/// `cli-errors/missing-options-create` — argv `api create-worklog`.
///
/// All five required options reported at once, where clap reports the first. The
/// order is `ApiCommand.kt`'s declaration order.
#[test]
fn create_worklog_with_no_options_reports_all_five_missing_ones_at_once() {
    matches_error_capture("missing-options-create", &["api", "create-worklog"]);
}

/// `cli-errors/missing-options-create-partial` — argv
/// `api create-worklog -h 8 -d 2026-09-18`.
///
/// The remaining three print in **declaration** order — `--project-id`, `--task`,
/// `--billability` — not alphabetically and not in the order the satisfied options
/// were typed. `--hours` and `--date` were given out of order on purpose.
#[test]
fn the_remaining_missing_options_print_in_declaration_order_not_in_typed_order() {
    matches_error_capture(
        "missing-options-create-partial",
        &["api", "create-worklog", "-h", "8", "-d", "2026-09-18"],
    );
}

/// `cli-errors/hours-short-flag-is-not-help` — argv `api create-worklog -h`.
///
/// C21 on the running binary rather than argued from source: `-h` here is
/// `--hours`, so it answers `Error: option -h requires a value` and then lists all
/// five missing options — including `--hours` itself, which was named but never
/// given a value.
#[test]
fn short_h_on_create_worklog_is_hours_and_demands_a_value_rather_than_printing_help() {
    matches_error_capture(
        "hours-short-flag-is-not-help",
        &["api", "create-worklog", "-h"],
    );
}

/// `cli-errors/missing-argument-delete` — argv `api delete-worklog`.
///
/// The only missing-**argument** case in the tree, since `delete-worklog` is the
/// only command with a positional.
#[test]
fn delete_worklog_without_its_positional_id_reports_a_missing_argument() {
    matches_error_capture("missing-argument-delete", &["api", "delete-worklog"]);
}

// ---------------------------------------------------------------------------
// Beyond the captures — help given an attached value
//
// Measured 2026-09-22 against the pinned incumbent and recorded at
// ~/.cache/tt-devpro-rewrite/measurements/clikt/help-with-attached-value.md.
// ---------------------------------------------------------------------------

/// `help-with-attached-value.md`, case `--help=x`.
///
/// Help is an ordinary valueless flag when it carries an attached value, so it
/// takes the `does not take a value` line rather than firing. A port that special-
/// cased `--help` before looking at the `=` would print help and exit 0.
#[test]
fn root_help_with_an_attached_value_is_a_usage_error_rather_than_help() {
    fails_with(
        &["--help=x"],
        USAGE_ROOT,
        &["option --help does not take a value"],
    );
}

/// `help-with-attached-value.md`, case `settle --help=x`. The same rule under a
/// leaf command, and it reprints **settle's** usage line.
#[test]
fn settle_help_with_an_attached_value_is_a_usage_error_under_settles_usage_line() {
    fails_with(
        &["settle", "--help=x"],
        USAGE_SETTLE,
        &["option --help does not take a value"],
    );
}

/// `help-with-attached-value.md`, case `api --help=x`. The same under a group.
#[test]
fn api_help_with_an_attached_value_is_a_usage_error_under_the_groups_usage_line() {
    fails_with(
        &["api", "--help=x"],
        USAGE_API,
        &["option --help does not take a value"],
    );
}

/// `help-with-attached-value.md`, case `api get-projects --help=x`. The same two
/// levels down.
#[test]
fn nested_help_with_an_attached_value_is_a_usage_error_under_the_leafs_usage_line() {
    fails_with(
        &["api", "get-projects", "--help=x"],
        USAGE_GET_PROJECTS,
        &["option --help does not take a value"],
    );
}

/// Measured 2026-09-22 (this run) — argv `api create-worklog --help=x`.
///
/// The help failure does **not** suppress the missing-option pass: six lines come
/// out, the attached-value error and then all five missing options. This is the
/// rule the measurement file does not reach, because none of its four commands has
/// a required option.
#[test]
fn a_help_with_an_attached_value_still_lets_the_missing_option_pass_run() {
    fails_with(
        &["api", "create-worklog", "--help=x"],
        USAGE_CREATE_WORKLOG,
        &[
            "option --help does not take a value",
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
            "missing option --hours",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `api get-worklogs --help=x`. The same
/// pairing with a single required option, so the two-line shape is unambiguous.
#[test]
fn a_help_with_an_attached_value_reports_the_one_missing_option_after_it() {
    fails_with(
        &["api", "get-worklogs", "--help=x"],
        USAGE_GET_WORKLOGS,
        &[
            "option --help does not take a value",
            "missing option --date",
        ],
    );
}

/// `help-with-attached-value.md`, case `--help x`.
///
/// The contrast that makes the four cases above mean something: a **separate**
/// token after `--help` is not an attached value, help fires, and the trailing `x`
/// is never looked at. Exit 0, root help on stdout.
#[test]
fn a_separate_token_after_help_is_not_an_attached_value_and_help_still_fires() {
    prints_the_help_of(&["--help", "x"], "help");
}

// ---------------------------------------------------------------------------
// Beyond the captures — the short-option cluster rule
//
// `-h=x` is in help-with-attached-value.md; the rest were measured in this run
// against the pinned incumbent, both streams and exit code.
// ---------------------------------------------------------------------------

/// `help-with-attached-value.md`, case `-h=x` — `Error: no such option -=`.
///
/// `-h` does not take a value at the root, so `-h=x` is the three letters `h`,
/// `=`, `x`; `h` is help and `=` is not an option. The incumbent reports the
/// unknown letter and exits 1 **although help already fired earlier in the same
/// cluster**, because Clikt's `parseShortOpt` throws before returning the cluster's
/// invocations and the `-h` invocation is discarded with it.
///
/// This is the sharpest case in the file: a port that fires help eagerly per letter
/// prints the root help and exits 0.
#[test]
fn an_unknown_letter_after_h_in_one_cluster_cancels_the_help_that_already_fired() {
    fails_with(&["-h=x"], USAGE_ROOT, &["no such option -="]);
}

/// Measured 2026-09-22 (this run) — argv `-hx`.
///
/// The same rule with no `=` in sight, which is what proves the rule is about the
/// unknown letter and not about the `=`. A fix keyed on `=` passes the case above
/// and fails this one.
#[test]
fn an_unknown_letter_cancels_its_cluster_even_without_an_equals_sign() {
    fails_with(&["-hx"], USAGE_ROOT, &["no such option -x"]);
}

/// Measured 2026-09-22 (this run) — argv `settle -hz`.
///
/// The same one level down, under a command whose `-h` *is* help. Catches a fix
/// applied only at the root.
#[test]
fn the_cluster_cancellation_rule_holds_under_a_subcommand_too() {
    fails_with(&["settle", "-hz"], USAGE_SETTLE, &["no such option -z"]);
}

/// Measured 2026-09-22 (this run) — argv `-xh`.
///
/// Contrast: the unknown letter comes **first**, so nothing had fired to cancel.
/// One message, not two, and the scan stops at `-x` rather than going on to report
/// `h` as well.
#[test]
fn an_unknown_letter_before_h_reports_once_and_stops_scanning_the_cluster() {
    fails_with(&["-xh"], USAGE_ROOT, &["no such option -x"]);
}

/// Measured 2026-09-22 (this run) — argv `-h x`.
///
/// Contrast: `-h` in a cluster of its own fires normally. Without this, "cancel the
/// cluster" could be over-corrected into "`-h` never fires in short form" and every
/// other test here would still pass.
#[test]
fn a_short_h_alone_still_prints_help_and_exits_zero() {
    prints_the_help_of(&["-h", "x"], "help");
}

/// Measured 2026-09-22 (this run) — argv `api get-projects -hd`.
///
/// Contrast, and the sharpest of the three: `-d` is left without a value, and help
/// still fires. **Only an unknown letter cancels a cluster**; a `requires a value`
/// failure in the same cluster does not. A port that cancelled on any error in the
/// cluster would print `option -d requires a value` here and look reasonable doing
/// it.
#[test]
fn a_valueless_option_later_in_the_cluster_does_not_cancel_the_help_that_fired() {
    prints_the_help_of(&["api", "get-projects", "-hd"], "api-get-projects-help");
}

/// Measured 2026-09-22 (this run) — argv `settle --nosuchflag --help`.
///
/// Contrast at the token level: an unknown **long** option in a *separate* token
/// does not cancel help. `--nosuchflag` is recorded as an error, `--help`'s own
/// token parses cleanly, and the eager pass finds its invocation — so help wins and
/// the run exits 0. This is the boundary that stops the cluster rule being
/// generalised into "any error anywhere suppresses help".
#[test]
fn an_unknown_long_option_in_a_separate_token_does_not_cancel_help() {
    prints_the_help_of(&["settle", "--nosuchflag", "--help"], "settle-help");
}

/// Measured 2026-09-22 (this run) — argv `settle --help=x --nosuchflag`.
///
/// And the same when help has already lost its eagerness to an attached value: both
/// errors come out, in token order.
#[test]
fn a_help_that_lost_its_eagerness_still_reports_the_later_unknown_option() {
    fails_with(
        &["settle", "--help=x", "--nosuchflag"],
        USAGE_SETTLE,
        &[
            "option --help does not take a value",
            "no such option --nosuchflag",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `api create-worklog -h --help`.
///
/// C21 at its most surprising: `-h` is `--hours`, it takes a value, and the value
/// it takes is the literal string `--help`. So help never fires and only **four**
/// options are missing — `--hours` is satisfied. A port that let clap own `-h`, or
/// that refused a hyphenated value, reports five.
#[test]
fn short_h_on_create_worklog_swallows_a_following_help_as_its_value() {
    fails_with(
        &["api", "create-worklog", "-h", "--help"],
        USAGE_CREATE_WORKLOG,
        &[
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `api create-worklog -hz`.
///
/// The same option taking the **rest of its own cluster** as the value: `-hz` is
/// `--hours z`, not `-h` followed by an unknown `-z`. Four missing options again,
/// and no `no such option -z`.
#[test]
fn a_value_taking_short_option_swallows_the_rest_of_its_cluster() {
    fails_with(
        &["api", "create-worklog", "-hz"],
        USAGE_CREATE_WORKLOG,
        &[
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `api update-worklog -h`.
///
/// C21 on the second write subcommand, which the captures cover only for
/// `create-worklog`. Seven lines, and `--id` leads the missing list because
/// `update-worklog` declares it first (`Models.kt:69-81`, C18).
#[test]
fn short_h_on_update_worklog_is_also_hours_and_id_leads_its_missing_list() {
    fails_with(
        &["api", "update-worklog", "-h"],
        USAGE_UPDATE_WORKLOG,
        &[
            "option -h requires a value",
            "missing option --id",
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
            "missing option --hours",
        ],
    );
}

// ---------------------------------------------------------------------------
// Beyond the captures — the rest of the failure surface
// ---------------------------------------------------------------------------

/// Measured 2026-09-22 (this run) — argv `api get-projects --dat 2026-01-01`.
///
/// The near-miss surface for *options*, which the captures cover only for
/// subcommands. The suggestion list carries both spellings —
/// `(Possible options: --date, -d)` — so a port that scored only long forms would
/// drop the `-d`.
#[test]
fn a_near_miss_long_option_lists_both_its_spellings() {
    fails_with(
        &["api", "get-projects", "--dat", "2026-01-01"],
        USAGE_GET_PROJECTS,
        &["no such option --dat. (Possible options: --date, -d)"],
    );
}

/// Measured 2026-09-22 (this run) — argv `settle --hel`.
///
/// A near miss that scores **two** names prints them as a list —
/// `(Possible options: --help, -h)` — where a single near miss prints
/// `Did you mean X?`. Both forms are live and the suggester picks between them by
/// how many names cleared its threshold.
///
/// Here `-h` is help's own short letter, because `settle` leaves it free.
#[test]
fn a_near_miss_of_help_lists_both_of_helps_spellings_where_short_h_is_free() {
    fails_with(
        &["settle", "--hel"],
        USAGE_SETTLE,
        &["no such option --hel. (Possible options: --help, -h)"],
    );
}

/// Measured 2026-09-22 (this run) — argv `api create-worklog --hel`.
///
/// **This one is a mutation survivor closed.** The same suggestion list on a
/// command where `-h` belongs to `--hours` and help therefore has *no* short form —
/// so `-h` appears exactly once, contributed by `--hours`, and the line is
/// character-for-character the one above.
///
/// It is the only invocation that can tell the two apart. Making help keep `-h`
/// unconditionally (C21 inverted) leaves every other case in this file unchanged,
/// because the cluster walk finds `--hours` first either way; what it changes is
/// this list, which would then carry `-h` **twice**. That mutation survived the
/// first round, and this test is what kills it.
#[test]
fn a_near_miss_of_help_names_short_h_once_even_where_hours_owns_it() {
    fails_with(
        &["api", "create-worklog", "--hel"],
        USAGE_CREATE_WORKLOG,
        &[
            "no such option --hel. (Possible options: --help, -h)",
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
            "missing option --hours",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `api create-worklog --hour`.
///
/// The single-candidate form of the same surface: `Did you mean --hours?`, not a
/// list. Pairs with the two above so that a port cannot satisfy all three by always
/// emitting one shape.
#[test]
fn a_near_miss_with_one_candidate_asks_did_you_mean_rather_than_listing() {
    fails_with(
        &["api", "create-worklog", "--hour"],
        USAGE_CREATE_WORKLOG,
        &[
            "no such option --hour. Did you mean --hours?",
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
            "missing option --hours",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `api create-worklog -q`.
///
/// An unknown letter in a cluster of its own carries no suggestion, and — unlike
/// the excess-argument pass — it does **not** suppress finalization: all five
/// missing options still follow it.
#[test]
fn an_unknown_short_letter_carries_no_suggestion_and_suppresses_no_missing_option() {
    fails_with(
        &["api", "create-worklog", "-q"],
        USAGE_CREATE_WORKLOG,
        &[
            "no such option -q",
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
            "missing option --hours",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `--version`.
///
/// There is no version flag, and there never was. Catches a port that let clap
/// attach its own `--version`, which would print a version string and exit 0 where
/// the incumbent fails.
#[test]
fn there_is_no_version_option_and_asking_for_one_is_an_unknown_option() {
    fails_with(&["--version"], USAGE_ROOT, &["no such option --version"]);
}

/// Measured 2026-09-22 (this run) — argv `settle extra-arg`.
///
/// `settle` declares no positionals and has no subcommands, so one excess token is
/// already an extra argument — **singular** noun, name in parentheses.
#[test]
fn one_excess_token_under_a_command_without_subcommands_is_a_single_extra_argument() {
    fails_with(
        &["settle", "extra-arg"],
        USAGE_SETTLE,
        &["got unexpected extra argument (extra-arg)"],
    );
}

/// Measured 2026-09-22 (this run) — argv `api nosuchsub anotherone`.
///
/// Two excess tokens under a group are extra arguments, **plural**, space-joined —
/// not a subcommand miss. The contrast with `cli-errors/unknown-subcommand`, which
/// is the same command with one token, is the whole point.
#[test]
fn two_excess_tokens_under_a_group_are_extra_arguments_not_a_subcommand_miss() {
    fails_with(
        &["api", "nosuchsub", "anotherone"],
        USAGE_API,
        &["got unexpected extra arguments (nosuchsub anotherone)"],
    );
}

/// Measured 2026-09-22 (this run) — argv `settle --json=yes`.
///
/// A declared flag rejects an attached value the same way `--help` does. Catches a
/// port that parsed `--json=yes` as truthy and then **ran the command**, which on
/// this flag means reaching the portal.
#[test]
fn a_boolean_flag_given_an_attached_value_is_a_usage_error() {
    fails_with(
        &["settle", "--json=yes"],
        USAGE_SETTLE,
        &["option --json does not take a value"],
    );
}

/// Measured 2026-09-22 (this run) — argv `settle --include-today=yes`.
///
/// The same on the second of settle's flags, and the one where being wrong is
/// worst: `--include-today` moves C1's cutoff onto an unfinished day.
#[test]
fn include_today_given_an_attached_value_is_a_usage_error_too() {
    fails_with(
        &["settle", "--include-today=yes"],
        USAGE_SETTLE,
        &["option --include-today does not take a value"],
    );
}

/// Measured 2026-09-22 (this run) — argv `api get-projects -zd 2026-01-01`.
///
/// One message, not one per remaining letter: the scan stops at the unknown `-z`
/// and never reports `-d`. Catches a port that collects an error per letter.
#[test]
fn an_unknown_first_letter_reports_once_and_never_reaches_the_rest_of_the_cluster() {
    fails_with(
        &["api", "get-projects", "-zd", "2026-01-01"],
        USAGE_GET_PROJECTS,
        &["no such option -z"],
    );
}

// ---------------------------------------------------------------------------
// Which finalization pass survives a token-pass error
//
// `~/.cache/tt-devpro-rewrite/measurements/clikt/finalization-passes.md`, 16 argv
// captures of the pinned incumbent, plus three rows measured in this run that the
// file does not carry.
//
// The rule, in one sentence: **the missing-required-argument and
// missing-required-option passes always run; the excess pass and the descent into a
// subcommand do not.** And the excess pass is not merely suppressed — a *single*
// excess token under a group produces a subcommand miss that **replaces** every
// message the token pass collected, while with *two* excess tokens it is the
// extra-arguments line that yields instead. Both halves of that asymmetry are
// pinned below, and so is each contrast row, because the failure mode here is a fix
// that over-corrects in one direction and looks right.
// ---------------------------------------------------------------------------

/// `finalization-passes.md`, argv `api delete-worklog --nosuchopt`.
///
/// The missing-required-**argument** pass runs although the token pass failed:
/// `no such option --nosuchopt` **and** `missing argument <id>`. `delete-worklog`
/// is the only command in the tree with a positional, so it is the only place the
/// two halves of the argument pass can be told apart — which is exactly why an
/// earlier port dropped this line and every other test still passed.
#[test]
fn a_token_error_leaves_the_missing_argument_pass_running() {
    fails_with(
        &["api", "delete-worklog", "--nosuchopt"],
        USAGE_DELETE_WORKLOG,
        &["no such option --nosuchopt", "missing argument <id>"],
    );
}

/// `finalization-passes.md`, argv `api delete-worklog --help=x`.
///
/// The same with a different token error, so the rule is about the *pass* and not
/// about `no such option` in particular.
#[test]
fn a_help_with_an_attached_value_also_leaves_the_missing_argument_pass_running() {
    fails_with(
        &["api", "delete-worklog", "--help=x"],
        USAGE_DELETE_WORKLOG,
        &[
            "option --help does not take a value",
            "missing argument <id>",
        ],
    );
}

/// `finalization-passes.md`, argv `api delete-worklog --nosuchopt aaa bbb`.
///
/// The other half: the **excess** pass does not run. `bbb` is one token too many
/// and is never mentioned; `<id>` took `aaa`, so nothing is missing either, and the
/// unknown option is the only line.
///
/// Together with the two above, this is what stops a fix going either way — running
/// both passes would add `got unexpected extra argument (bbb)` here, running
/// neither would drop `missing argument <id>` there.
#[test]
fn a_token_error_still_cancels_the_excess_argument_pass() {
    fails_with(
        &["api", "delete-worklog", "--nosuchopt", "aaa", "bbb"],
        USAGE_DELETE_WORKLOG,
        &["no such option --nosuchopt"],
    );
}

/// `finalization-passes.md`, argv `api delete-worklog aaa bbb --nosuchopt`.
///
/// Order independence: the excess pass is cancelled by a token error that arrives
/// **after** the excess tokens, not only by one that precedes them. A port that
/// cancelled on a flag set during the walk rather than on "the token pass produced
/// a message" would pass the case above and fail this one.
#[test]
fn the_excess_pass_is_cancelled_by_a_token_error_that_comes_after_the_excess() {
    fails_with(
        &["api", "delete-worklog", "aaa", "bbb", "--nosuchopt"],
        USAGE_DELETE_WORKLOG,
        &["no such option --nosuchopt"],
    );
}

/// `finalization-passes.md`, argv `api delete-worklog --help=x someid`.
///
/// The control for all four above: with `<id>` satisfied by a normal token there is
/// nothing missing and nothing in excess, so one line. This is what makes the
/// *absence* of a `missing argument` line meaningful rather than accidental.
#[test]
fn a_satisfied_positional_leaves_only_the_token_error_behind() {
    fails_with(
        &["api", "delete-worklog", "--help=x", "someid"],
        USAGE_DELETE_WORKLOG,
        &["option --help does not take a value"],
    );
}

/// `finalization-passes.md`, argv `api no-such-thing --nosuchflag`.
///
/// **A single excess token under a group replaces what the token pass collected.**
/// The incumbent prints `no such subcommand no-such-thing` and *not*
/// `no such option --nosuchflag` — two different single lines, both exit 1, with
/// nothing about the shape to give the difference away.
///
/// This is not "the excess pass is suppressed": the subcommand miss wins outright.
#[test]
fn a_single_excess_token_replaces_a_token_error_that_came_after_it() {
    fails_with(
        &["api", "no-such-thing", "--nosuchflag"],
        USAGE_API,
        &["no such subcommand no-such-thing"],
    );
}

/// `finalization-passes.md`, argv `api --nosuchflag nosuchsub`.
///
/// The same replacement from the other side — the token error comes **first** and
/// is still replaced. So it is the subcommand miss that wins, not whichever message
/// came last.
#[test]
fn a_single_excess_token_replaces_a_token_error_that_came_before_it() {
    fails_with(
        &["api", "--nosuchflag", "nosuchsub"],
        USAGE_API,
        &["no such subcommand nosuchsub"],
    );
}

/// `finalization-passes.md`, argv `api nosuchsub --help=x`.
///
/// The replacement swallows a help-with-an-attached-value error too, which is the
/// one case where a port might reasonably expect help's error to be special.
#[test]
fn a_single_excess_token_replaces_even_a_help_value_error() {
    fails_with(
        &["api", "nosuchsub", "--help=x"],
        USAGE_API,
        &["no such subcommand nosuchsub"],
    );
}

/// `finalization-passes.md`, argv `api --nosuchflag nosuchsub anotherone`.
///
/// **Two excess tokens behave the opposite way**: the extra-arguments line is the
/// one that yields, and the token error survives. A port that made the excess pass
/// uniformly win or uniformly lose fails either this or the three above.
#[test]
fn two_excess_tokens_yield_to_a_token_error_instead_of_replacing_it() {
    fails_with(
        &["api", "--nosuchflag", "nosuchsub", "anotherone"],
        USAGE_API,
        &["no such option --nosuchflag"],
    );
}

/// `finalization-passes.md`, argv `api nosuchsub anotherone --nosuchflag`.
///
/// And the same with the token error last, so the two-token rule is order-
/// independent in the same way the one-token rule is.
#[test]
fn two_excess_tokens_yield_whichever_side_the_token_error_arrives_from() {
    fails_with(
        &["api", "nosuchsub", "anotherone", "--nosuchflag"],
        USAGE_API,
        &["no such option --nosuchflag"],
    );
}

/// Measured 2026-09-22 (this run); not in `finalization-passes.md` — argv
/// `api nosuchsub -h`.
///
/// The contrast that bounds the whole replacement rule: **help still beats it.**
/// `nosuchsub` is an excess token that would otherwise produce a subcommand miss,
/// and the `-h` after it prints api's help and exits 0 instead. Pairs with
/// `an_unrecognised_token_does_not_stop_a_later_help_from_firing`, which is the
/// long spelling; this one also proves `-h` is free on the `api` group.
#[test]
fn a_short_help_after_an_excess_token_still_beats_the_subcommand_miss() {
    prints_the_help_of(&["api", "nosuchsub", "-h"], "api-help");
}

/// `finalization-passes.md`, argv `settle extra --nosuchflag`.
///
/// The no-positionals control for the argument pass: `settle` declares none, so
/// there is nothing required to be missing and the excess token is cancelled —
/// exactly one line. Without this, "the missing-argument pass always runs" could be
/// misread as "a command with no positionals invents one".
#[test]
fn a_command_with_no_positionals_has_nothing_to_report_missing() {
    fails_with(
        &["settle", "extra", "--nosuchflag"],
        USAGE_SETTLE,
        &["no such option --nosuchflag"],
    );
}

/// `finalization-passes.md`, argv `api get-worklogs --nosuchopt`.
///
/// The missing-**option** pass on a command with exactly one required option, so
/// the two-line shape is unambiguous. Pairs with
/// `a_token_error_is_printed_before_the_missing_options_it_did_not_suppress`,
/// which is the five-option form.
#[test]
fn a_token_error_leaves_a_single_missing_option_reported_after_it() {
    fails_with(
        &["api", "get-worklogs", "--nosuchopt"],
        USAGE_GET_WORKLOGS,
        &["no such option --nosuchopt", "missing option --date"],
    );
}

/// Measured 2026-09-22 (this run) — argv `settle --dry-run extra --nosuchflag`.
///
/// The excess token `extra` is suppressed by the later unknown option, and
/// `--dry-run` — which would otherwise reach Chrono and the portal — never runs.
#[test]
fn an_excess_token_is_suppressed_by_a_later_unknown_option() {
    fails_with(
        &["settle", "--dry-run", "extra", "--nosuchflag"],
        USAGE_SETTLE,
        &["no such option --nosuchflag"],
    );
}

/// Measured 2026-09-22 (this run) — argv `api --nosuch get-projects`.
///
/// The subcommand is resolved in that same suppressed pass, so the failure is
/// reported against **`api`'s** usage line and `get-projects` never runs. Catches a
/// port that descended first and blamed the child.
#[test]
fn a_parent_level_unknown_option_is_blamed_on_the_parent_and_stops_the_descent() {
    fails_with(
        &["api", "--nosuch", "get-projects"],
        USAGE_API,
        &["no such option --nosuch"],
    );
}

/// Measured 2026-09-22 (this run) — argv `api create-worklog --nosuchopt`.
///
/// Token-pass errors come out in token order and finalization errors after them:
/// the unknown option, then the five missing ones. Six lines where clap prints one.
#[test]
fn a_token_error_is_printed_before_the_missing_options_it_did_not_suppress() {
    fails_with(
        &["api", "create-worklog", "--nosuchopt"],
        USAGE_CREATE_WORKLOG,
        &[
            "no such option --nosuchopt",
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
            "missing option --hours",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `api update-worklog --nosuchopt`.
///
/// The same on the seven-option subcommand, which is the longest error block the
/// tool emits and the one where an ordering slip is easiest to miss.
#[test]
fn the_longest_missing_option_block_keeps_its_declaration_order() {
    fails_with(
        &["api", "update-worklog", "--nosuchopt"],
        USAGE_UPDATE_WORKLOG,
        &[
            "no such option --nosuchopt",
            "missing option --id",
            "missing option --date",
            "missing option --project-id",
            "missing option --task",
            "missing option --billability",
            "missing option --hours",
        ],
    );
}

/// Measured 2026-09-22 (this run) — argv `api nosuchsub --help`.
///
/// An unrecognised token does not end the scan: `nosuchsub` became a positional,
/// the loop ran on, and the `--help` after it fired. **api's** help, exit 0, and no
/// `no such subcommand` line at all.
#[test]
fn an_unrecognised_token_does_not_stop_a_later_help_from_firing() {
    prints_the_help_of(&["api", "nosuchsub", "--help"], "api-help");
}

/// Measured 2026-09-22 (this run) — argv `--help api`.
///
/// Help belongs to the deepest command reached **before** it. `api` comes after, so
/// the root's help prints and `api` is never entered.
#[test]
fn help_before_a_subcommand_name_prints_the_parents_help_not_the_childs() {
    prints_the_help_of(&["--help", "api"], "help");
}

/// Measured 2026-09-22 (this run) — argv `api --help get-projects`.
///
/// The same one level down, and the sharper half: `api`'s help, not
/// `get-projects`'. Together with the case above this pins the direction of the
/// rule, which a port could get exactly backwards and still print *a* help text.
#[test]
fn help_between_a_group_and_its_subcommand_prints_the_groups_help() {
    prints_the_help_of(&["api", "--help", "get-projects"], "api-help");
}

/// Measured 2026-09-22 (this run) — argv `api get-projects -d --help`.
///
/// C33 in its short spelling and with a hyphenated value: `-d` takes the literal
/// string `--help` as its date, so help never fires and the date conversion fails
/// on it. The usage line, the blank line, the empty stdout and the exit code are
/// the incumbent's; only the tail is chrono's.
///
/// No capture file exists for this one, so the incumbent's side is the measurement
/// recorded in this test rather than a file on disk.
#[test]
fn a_hyphenated_value_taken_by_short_d_fails_date_conversion_rather_than_printing_help() {
    let got = run(&["api", "get-projects", "-d", "--help"]);
    assert_eq!(show(&got.stdout), "");
    assert_eq!(
        show(&got.stderr),
        usage_block(
            USAGE_GET_PROJECTS,
            &["invalid value for -d: --help is not a date in YYYY-MM-DD form"],
        )
    );
    assert_eq!(got.status.code(), Some(1));
}

/// Measured 2026-09-22 (this run) — argv `api get-projects -dh=x`.
///
/// The cluster rule and the value rule meeting: `-d` takes a value, so it swallows
/// `h=x` whole rather than reading `h` as help and `=` as an unknown letter. The
/// incumbent says `Text 'h=x' could not be parsed at index 0`; the port says
/// `h=x is not a date in YYYY-MM-DD form`, C33's divergence again.
///
/// This is the case that separates "a cluster is letters" from "a cluster is
/// letters until one of them takes a value".
#[test]
fn a_value_taking_short_option_swallows_an_equals_sign_instead_of_splitting_on_it() {
    let got = run(&["api", "get-projects", "-dh=x"]);
    assert_eq!(show(&got.stdout), "");
    assert_eq!(
        show(&got.stderr),
        usage_block(
            USAGE_GET_PROJECTS,
            &["invalid value for -d: h=x is not a date in YYYY-MM-DD form"],
        )
    );
    assert_eq!(got.status.code(), Some(1));
}

// ---------------------------------------------------------------------------
// Stream discipline — C7 at the process level
// ---------------------------------------------------------------------------

/// C7, as far as a network-free test can carry it: **help is stdout, failures are
/// stderr, and neither ever writes to the other.**
///
/// Runs every argv this file uses that is known to succeed or to fail in the
/// parser, and asserts the empty stream really is empty. The per-case tests above
/// each assert this for their own case; this one asserts it as a property, so a
/// port that started echoing a failure to both streams fails one test with a name
/// that says what happened rather than twenty with names that do not.
///
/// The rest of C7 — `--json` keeping stdout machine-clean while notices go to
/// stderr — needs a run that reaches Chrono and the portal, and belongs to the
/// step-6 differential harness.
#[test]
fn help_never_touches_stderr_and_a_usage_failure_never_touches_stdout() {
    let prints_help: [&[&str]; 6] = [
        &[],
        &["--help"],
        &["api"],
        &["settle", "--help"],
        &["api", "nosuchsub", "--help"],
        &["api", "get-projects", "-hd"],
    ];
    for argv in prints_help {
        let got = run(argv);
        assert_eq!(show(&got.stderr), "", "argv {argv:?} wrote to stderr");
        assert!(!got.stdout.is_empty(), "argv {argv:?} wrote no help");
        assert_eq!(got.status.code(), Some(0), "argv {argv:?} did not exit 0");
    }

    let fails_in_the_parser: [&[&str]; 8] = [
        &["--version"],
        &["-h=x"],
        &["settle", "--nosuchflag"],
        &["settle", "--json=yes"],
        &["api", "no-such-thing"],
        &["api", "get-worklogs"],
        &["api", "create-worklog"],
        &["api", "delete-worklog"],
    ];
    for argv in fails_in_the_parser {
        let got = run(argv);
        assert_eq!(show(&got.stdout), "", "argv {argv:?} wrote to stdout");
        assert!(!got.stderr.is_empty(), "argv {argv:?} wrote no error");
        assert_eq!(got.status.code(), Some(1), "argv {argv:?} did not exit 1");
    }
}

/// C33 — **exit 1 is the only non-zero code this surface emits**, and it is not
/// clap's 2.
///
/// clap's `Error::exit_code()` returns the constant `USAGE_CODE = 2` whenever the
/// error goes to stderr, and offers no builder method, derive attribute or override
/// to change it. The only way to 1 is never to call `parse()`. This test is what
/// notices the day someone simplifies `try_parse()` back into `parse()`: the output
/// would still look plausible and every byte-compare above would fail with a
/// confusing message, while this one names the cause.
#[test]
fn every_parse_failure_exits_one_and_never_claps_own_two() {
    let failing: [&[&str]; 10] = [
        &["--version"],
        &["-h=x"],
        &["-hx"],
        &["settle", "--nosuchflag"],
        &["settle", "extra-arg"],
        &["settle", "--from", "notadate", "--dry-run"],
        &["api", "no-such-thing"],
        &["api", "create-worklog-typo"],
        &["api", "get-projects", "--date", "nope"],
        &["api", "delete-worklog"],
    ];
    for argv in failing {
        assert_eq!(
            run(argv).status.code(),
            Some(1),
            "argv {argv:?} should exit 1, and clap's own default is 2"
        );
    }
}

/// The binary is deterministic across runs on this surface.
///
/// Cheap, and it guards C28's real hazard from the outside: a randomized `HashMap`
/// anywhere in the option or subcommand walk would make consecutive runs of the
/// same binary disagree, and that would read as a data change rather than as a bug.
/// The incumbent's `settle --dry-run` was measured byte-identical across two
/// consecutive runs; this is the network-free half of the same property.
#[test]
fn the_same_invocation_twice_produces_identical_bytes() {
    let argv: [&[&str]; 5] = [
        &["--help"],
        &["api", "--help"],
        &["api", "create-worklog"],
        &["api", "update-worklog", "--nosuchopt"],
        &["api", "get-projects", "--dat", "2026-01-01"],
    ];
    for tokens in argv {
        let first = run(tokens);
        let second = run(tokens);
        assert_eq!(
            show(&first.stdout),
            show(&second.stdout),
            "stdout differs between two runs of {tokens:?}"
        );
        assert_eq!(
            show(&first.stderr),
            show(&second.stderr),
            "stderr differs between two runs of {tokens:?}"
        );
        assert_eq!(
            first.status.code(),
            second.status.code(),
            "exit code differs between two runs of {tokens:?}"
        );
    }
}
