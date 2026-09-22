//! Command surfaces and the pure rules they are built on.
//!
//! This module also carries the CLI's *failure* surface, because Clikt renders it
//! and clap does not render it the same way. Three things had to be transcribed
//! rather than configured, and all three were read out of `clikt-jvm` 4.2.2 and
//! measured against the pinned incumbent (`~/.cache/tt-devpro-rewrite/tt-devpro.kotlin-incumbent`).
//!
//! **The shape.** A usage failure prints nothing on stdout; on stderr it prints the
//! usage line of the deepest command reached, a blank line, then one `Error: ` line
//! per problem, and exits 1. `api create-worklog` with no options prints *five*
//! error lines, one per missing option, in declaration order — clap reports the same
//! set but as a single message, so the lines are rebuilt here.
//!
//! **The suggestion.** `Localization.noSuchOption` / `noSuchSubcommand` take a list
//! of possibilities and render nothing for zero, `. Did you mean <x>?` for one, and
//! `. (Possible options: a, b)` for more. The list comes from
//! `ContextKt$DEFAULT_CORRECTION_SUGGESTOR`: Jaro-Winkler similarity against every
//! candidate, keep `> 0.8` strictly, sort descending, stably.
//!
//! **The similarity is not the textbook one.** `JaroWinkerSimilarityKt` (the typo is
//! upstream's) uses the *full* common prefix rather than capping it at four
//! characters, so `create-worklog-typo` against `create-worklog` scores exactly 1.0
//! where the standard formula gives 0.956. `Math.min(_, 1.0)` is what keeps it in
//! range, which is why [`crate::service::normalizer::java_min`] is used rather than
//! `f64::min`. Truth table: `~/.cache/tt-devpro-rewrite/measurements/clikt/jw-truth-table.tsv`,
//! produced by calling the jar's own function (`JwProbe.java` beside it).

pub mod api;
pub mod holidays;
pub mod settle;
pub mod settle_render;
pub mod settle_window;

use crate::service::normalizer::java_min;

/// What a command body did, which under D3 is also what the process exits with.
///
/// The incumbent catches every error, prints its message to stderr and **exits 0**
/// (C24). D3 keeps the message and changes the code, so a command that failed has
/// to say so to `main` without printing a second time — every one of those messages
/// is a measured byte sequence produced where the incumbent produces it, and an
/// `anyhow::Error` travelling up to `main` would be rendered again in a shape
/// nothing measured.
///
/// [`Failed`](Outcome::Failed) therefore means "the message is already on stderr".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed,
}

impl Outcome {
    /// The process exit code. `1` rather than `2`, which is C33's territory: clap
    /// exits 2 on a usage failure and the incumbent exits 1, so `main` renders that
    /// case itself and never reaches here.
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Ok => 0,
            Outcome::Failed => 1,
        }
    }
}

/// `java.time.LocalDate.parse`, which is `ISO_LOCAL_DATE` and is strict about every
/// character. Measured on GraalVM JDK 21.0.11 (`~/.cache/tt-devpro-rewrite/measurements/jdk/parse-date.tsv`):
/// `2026-1-15`, `2026-01-5`, `26-01-15`, `2026-01-15T00:00:00`, `" 2026-01-15"` and
/// `"2026-01-15 "` are all rejected, so `chrono`'s `%Y-%m-%d` cannot be used on its
/// own — it accepts single-digit months and days.
///
/// Calendar validity is checked too: `2026-02-30` and `2026-13-01` parse as shapes
/// and fail as dates, which is why the shape test and `from_ymd_opt` are both here.
///
/// **Named divergence: the extended year form.** Java accepts `+99999-01-01` (and
/// rejects the unprefixed `99999-01-01`). This port accepts four-digit years only.
/// The form exists for `ISO_LOCAL_DATE` completeness, not for a `--date` flag.
///
/// C33: the caller renders `invalid value for --<name>: ` and this message follows
/// it. Java's own tail is `DateTimeParseException.getMessage()` and is not
/// reproduced — reproducing it means hand-writing Java's message catalogue.
pub fn parse_iso_date(text: &str) -> Result<chrono::NaiveDate, String> {
    let bytes = text.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    if !shaped {
        return Err(format!("{text} is not a date in YYYY-MM-DD form"));
    }

    let year: i32 = text[0..4].parse().expect("four ascii digits");
    let month: u32 = text[5..7].parse().expect("two ascii digits");
    let day: u32 = text[8..10].parse().expect("two ascii digits");
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| format!("{text} is not a date on the calendar"))
}

/// `JaroWinkerSimilarityKt.jaroSimilarity`, transcribed instruction for instruction.
///
/// The transposition count is upstream's own and is not the textbook definition: it
/// increments when a matched index falls before the previously matched index, rather
/// than counting half the out-of-order pairs.
fn jaro_similarity(s1: &str, s2: &str) -> f64 {
    let a: Vec<char> = s1.chars().collect();
    let b: Vec<char> = s2.chars().collect();

    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a.len() == 1 && b.len() == 1 {
        return if a[0] == b[0] { 1.0 } else { 0.0 };
    }

    // `max(len1, len2) / 2 - 1`, in Java's integer division.
    let match_distance = (a.len().max(b.len()) / 2) as isize - 1;
    let mut b_matched = vec![false; b.len()];
    let mut matches = 0.0f64;
    let mut transpositions = 0usize;
    let mut last_match_index = 0usize;

    for (i, &c) in a.iter().enumerate() {
        let start = (i as isize - match_distance).max(0) as usize;
        let end = ((b.len() as isize - 1).min(i as isize + match_distance)).max(-1);
        if end < 0 {
            continue;
        }
        let end = end as usize;
        let mut j = start;
        while j <= end {
            if c == b[j] && !b_matched[j] {
                b_matched[j] = true;
                matches += 1.0;
                if j < last_match_index {
                    transpositions += 1;
                }
                last_match_index = j;
                break;
            }
            if j == end {
                break;
            }
            j += 1;
        }
    }

    if matches == 0.0 {
        return 0.0;
    }
    (matches / a.len() as f64
        + matches / b.len() as f64
        + (matches - transpositions as f64) / matches)
        / 3.0
}

/// `JaroWinkerSimilarityKt.jaroWinklerSimilarity`.
///
/// The prefix weight is applied to the **whole** common prefix. Upstream's four-
/// character cap is absent, which is what lets the result exceed 1.0 and makes the
/// final `min` load-bearing rather than defensive.
pub fn jaro_winkler_similarity(s1: &str, s2: &str) -> f64 {
    let prefix_len = s1
        .chars()
        .zip(s2.chars())
        .take_while(|(x, y)| x == y)
        .count();
    let jaro = jaro_similarity(s1, s2);
    java_min(jaro + 0.1 * prefix_len as f64 * (1.0 - jaro), 1.0)
}

/// `ContextKt.DEFAULT_CORRECTION_SUGGESTOR`: score every candidate, keep those
/// strictly above 0.8, best first. The sort is stable, so equally-scoring candidates
/// keep the order they were declared in.
pub fn suggest<'a>(entered: &str, possibilities: &[&'a str]) -> Vec<&'a str> {
    let mut scored: Vec<(&str, f64)> = possibilities
        .iter()
        .map(|&p| (p, jaro_winkler_similarity(entered, p)))
        .filter(|(_, score)| *score > 0.8)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.into_iter().map(|(name, _)| name).collect()
}

/// `Localization.noSuchOption`, including the three-way branch on how many
/// suggestions survived.
pub fn no_such_option(name: &str, possibilities: &[&str]) -> String {
    format!(
        "no such option {name}{}",
        suggestion_tail("options", possibilities)
    )
}

/// `Localization.noSuchSubcommand`.
pub fn no_such_subcommand(name: &str, possibilities: &[&str]) -> String {
    format!(
        "no such subcommand {name}{}",
        suggestion_tail("subcommands", possibilities)
    )
}

fn suggestion_tail(plural: &str, possibilities: &[&str]) -> String {
    match possibilities.len() {
        0 => String::new(),
        1 => format!(". Did you mean {}?", possibilities[0]),
        _ => format!(". (Possible {plural}: {})", possibilities.join(", ")),
    }
}

/// The usage line a command prints when it fails, which is the **first line of its
/// own help text** — measured, not assumed. `api get-projects --help` opens with
/// `Usage: tt-devpro api get-projects [<options>]`, and
/// `~/.cache/tt-devpro-rewrite/baseline/cli-errors/option-needs-value.err` opens with
/// the same bytes. That holds for the group commands too, whose usage carries
/// `<command> [<args>]...`, and for `delete-worklog`, whose usage carries `<id>`.
///
/// Deriving it rather than storing a second constant is the point: two constants can
/// disagree, and the disagreement would show up only on a failing invocation, which
/// is exactly where nobody is looking.
pub fn usage_line(help: &str) -> &str {
    help.split('\n').next().unwrap_or("")
}

/// The whole stderr body of a usage failure: the usage line, a blank line, then one
/// `Error: ` line per message. No trailing newline — the caller's `eprintln!` adds
/// the one the incumbent emits.
pub fn usage_error(usage: &str, messages: &[String]) -> String {
    let mut out = String::from(usage);
    out.push('\n');
    for message in messages {
        out.push_str("\nError: ");
        out.push_str(message);
    }
    out
}

// ---------------------------------------------------------------------------
// Kotlin's two ways of reading a number out of a string
// ---------------------------------------------------------------------------
//
// `ApiCommand.kt:120` uses `String.toDouble()` and `SettleCommand.kt:766` uses
// `String.toDoubleOrNull()`. They are the same parser: `toDoubleOrNull` screens
// the input against `ScreenFloatValueRegEx` and then calls `parseDouble` anyway,
// and the screen was measured to accept exactly what `parseDouble` accepts.
// Measured by calling the shipped `kotlin-stdlib-1.9.22.jar`'s own function over a
// 49-value corpus — `~/.cache/tt-devpro-rewrite/measurements/kotlin/NumProbe.java`
// and `toOrNull.out` — rather than read off the regex: `0x1p3` is accepted (8.0),
// `0x10` is not, `010` is decimal 10, `\u00a0` is not whitespace and `1_000` is
// not a number. So one parser serves both, and the hex divergence named in
// [`api`]'s `parse_hours` applies to the settle prompt too.

/// The measured `Double.parseDouble` surface, including its two error messages.
pub fn java_parse_double(raw: &str) -> Result<f64, String> {
    // `Double.parseDouble` strips every code unit `<= ' '`, then reports the
    // *stripped* string in its message: `" abc "` fails with `"abc"`.
    let trimmed = raw.trim_matches(|c: char| c <= ' ');
    if trimmed.is_empty() {
        return Err("empty String".to_string());
    }
    parse_trimmed_double(trimmed).ok_or_else(|| format!("For input string: \"{trimmed}\""))
}

fn parse_trimmed_double(trimmed: &str) -> Option<f64> {
    let (negative, body) = match trimmed.strip_prefix(['+', '-']) {
        Some(rest) => (trimmed.starts_with('-'), rest),
        None => (false, trimmed),
    };

    // These two are exact and take no type suffix, so they are settled first.
    if body == "Infinity" {
        return Some(if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    if body == "NaN" {
        return Some(f64::NAN);
    }
    let body = body.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(body);
    if body.is_empty() {
        return None;
    }
    // Everything Java spells with letters has been handled above, so any remaining
    // letter other than an exponent marker is Rust being the more permissive of the
    // two — `inf`, `nan` and friends land here and must be refused.
    //
    // It is **not** what refuses the hexadecimal form. An explicit `starts_with("0x")`
    // guard stood here until a mutation round deleted it with every test still green,
    // and a second mutation — relaxing this screen to let `x` and `p` through — also
    // left every test green. Both times the refusal came from the same place it always
    // did: `str::parse::<f64>` does not read hex floats either. The named divergence in
    // `parse_hours`' doc comment is therefore a property of the Rust parser rather than
    // of anything written here, and
    // `tests::a_hexadecimal_float_is_refused_although_the_jvm_reads_it` pins the
    // outcome without claiming a mechanism.
    if body
        .bytes()
        .any(|b| b.is_ascii_alphabetic() && b != b'e' && b != b'E')
    {
        return None;
    }

    let signed = if negative {
        format!("-{body}")
    } else {
        body.to_string()
    };
    signed.parse::<f64>().ok()
}

/// Kotlin's `String.toDoubleOrNull()` — `SettleCommand.kt:766`, the `New hours:`
/// prompt. Same accepted set as [`java_parse_double`], with `None` where that one
/// returns its message, because the caller has its own: `Invalid. Must be >= 0.25`.
pub fn kotlin_double_or_null(raw: &str) -> Option<f64> {
    let trimmed = raw.trim_matches(|c: char| c <= ' ');
    if trimmed.is_empty() {
        return None;
    }
    parse_trimmed_double(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of `jw-truth-table.tsv`, which came out of the jar's own function
    /// on GraalVM JDK 21.0.11. Exact equality, not a tolerance: the values are the
    /// JVM's own `double`s and a port that agrees to six places but not to the bit
    /// has got the arithmetic order wrong somewhere.
    #[test]
    fn the_similarity_reproduces_the_jar_bit_for_bit() {
        let rows: &[(&str, &str, f64)] = &[
            ("create-worklog-typo", "get-projects", 0.5553467000835423),
            ("create-worklog-typo", "get-worklogs", 0.7532163742690058),
            ("create-worklog-typo", "create-worklog", 1.0),
            ("create-worklog-typo", "update-worklog", 0.7276144907723855),
            ("create-worklog-typo", "delete-worklog", 0.7276144907723855),
            ("no-such-thing", "get-projects", 0.46367521367521364),
            ("no-such-thing", "get-worklogs", 0.38247863247863245),
            ("no-such-thing", "create-worklog", 0.4478021978021978),
            ("no-such-thing", "update-worklog", 0.4478021978021978),
            ("no-such-thing", "delete-worklog", 0.3705738705738706),
            ("abc", "abc", 1.0),
            ("a", "b", 0.0),
            ("martha", "marhta", 0.9611111111111111),
            ("dwayne", "duane", 0.8400000000000001),
            ("dixon", "dicksonx", 0.8133333333333332),
            ("settle", "settel", 0.9666666666666667),
            ("api", "api", 1.0),
            ("ap", "api", 0.9111111111111111),
            ("apii", "api", 0.9416666666666667),
            ("apu", "api", 0.8222222222222222),
            ("--nosuchflag", "--dry-run", 0.6222222222222222),
            ("--dry-runn", "--dry-run", 0.9966666666666667),
            ("CRABS", "crabs", 0.0),
            ("abcdefgh", "abcdefgi", 0.975),
            ("abcdefgh", "hgfedcba", 0.4166666666666667),
            ("xyz", "abcdefghij", 0.0),
            ("a", "ab", 0.8500000000000001),
            ("ab", "a", 0.8500000000000001),
            ("", "", 1.0),
            ("", "a", 0.0),
            ("a", "", 0.0),
            ("ab", "ab", 1.0),
            ("ab", "ba", 0.0),
            ("abc", "acb", 0.5999999999999999),
            ("aa", "aa", 1.0),
            ("aaa", "aa", 0.9111111111111111),
            ("create-worklog", "create-worklog-typo", 1.0),
            ("get-worklogs", "create-worklog-typo", 0.7532163742690058),
            ("settle", "setle", 0.9611111111111111),
            ("sette", "settle", 0.9666666666666667),
            ("api", "apo", 0.8222222222222222),
            ("zzz", "api", 0.0),
        ];
        for &(x, y, expected) in rows {
            assert_eq!(
                jaro_winkler_similarity(x, y),
                expected,
                "jaroWinklerSimilarity({x:?}, {y:?})"
            );
        }
    }

    /// The uncapped prefix is the whole difference from the textbook formula, and it
    /// is what makes the measured `create-worklog-typo` suggestion fire. With the
    /// standard four-character cap the score is 0.9555…, still above the 0.8 gate —
    /// so this test pins the value rather than the verdict, because the verdict
    /// cannot tell the two implementations apart.
    #[test]
    fn the_prefix_weight_is_uncapped_and_the_min_is_what_bounds_it() {
        let jaro = jaro_similarity("create-worklog-typo", "create-worklog");
        let prefix = 14.0;
        assert!(
            jaro + 0.1 * prefix * (1.0 - jaro) > 1.0,
            "the unbounded score must overshoot, or the min is decoration"
        );
        assert_eq!(
            jaro_winkler_similarity("create-worklog-typo", "create-worklog"),
            1.0
        );

        let capped_at_four = jaro + 0.1 * 4.0 * (1.0 - jaro);
        assert_ne!(
            capped_at_four, 1.0,
            "a capped prefix would not reach 1.0, which is how the two are told apart"
        );
    }

    /// The gate is `> 0.8` strictly (`dcmpl; ifle`), and the difference from `>=` is
    /// reachable: a search over 20 000 random pairs against the jar found 46 that
    /// score exactly 0.8, `("addcce", "adbd")` among them. Under `>=` that candidate
    /// would be offered as a suggestion.
    #[test]
    fn a_candidate_scoring_exactly_zero_point_eight_is_excluded() {
        assert_eq!(
            jaro_winkler_similarity("addcce", "adbd"),
            0.8,
            "the premise: this pair sits exactly on the line"
        );
        assert!(
            suggest("addcce", &["adbd"]).is_empty(),
            "the gate is > 0.8, not >="
        );

        assert_eq!(
            jaro_winkler_similarity("dixon", "dicksonx"),
            0.8133333333333332
        );
        assert_eq!(
            suggest("dixon", &["dicksonx"]),
            vec!["dicksonx"],
            "and the first value above the line does survive"
        );
    }

    /// The measured case: `api create-worklog-typo` suggested `create-worklog` and
    /// nothing else, although two other candidates share the `-worklog` suffix.
    #[test]
    fn the_measured_near_miss_suggests_exactly_one_subcommand() {
        let candidates = [
            "get-projects",
            "get-worklogs",
            "create-worklog",
            "update-worklog",
            "delete-worklog",
        ];
        assert_eq!(
            suggest("create-worklog-typo", &candidates),
            vec!["create-worklog"]
        );
        assert_eq!(
            no_such_subcommand(
                "create-worklog-typo",
                &suggest("create-worklog-typo", &candidates)
            ),
            "no such subcommand create-worklog-typo. Did you mean create-worklog?"
        );
    }

    /// The measured far miss: `api no-such-thing` suggested nothing, so the message
    /// has no tail at all.
    #[test]
    fn the_measured_far_miss_suggests_nothing_and_adds_no_tail() {
        let candidates = [
            "get-projects",
            "get-worklogs",
            "create-worklog",
            "update-worklog",
            "delete-worklog",
        ];
        assert!(suggest("no-such-thing", &candidates).is_empty());
        assert_eq!(
            no_such_subcommand("no-such-thing", &[]),
            "no such subcommand no-such-thing"
        );
    }

    /// Two or more survivors take the list form, not a repeated `Did you mean`.
    #[test]
    fn two_survivors_are_listed_rather_than_offered_singly() {
        assert_eq!(
            no_such_option("--dry-runn", &["--dry-run", "--dry-runs"]),
            "no such option --dry-runn. (Possible options: --dry-run, --dry-runs)"
        );
        assert_eq!(
            no_such_subcommand("aaa", &["aab", "aac"]),
            "no such subcommand aaa. (Possible subcommands: aab, aac)"
        );
    }

    /// Ordering is by descending score, and the sort is stable so that a tie keeps
    /// declaration order. `update-worklog` and `delete-worklog` tie exactly against
    /// `create-worklog-typo` at 0.7276…, which is the natural tie in this command
    /// set; they are below the gate, so the tie is shown on a pair that is not.
    #[test]
    fn suggestions_come_back_best_first_and_a_tie_keeps_declaration_order() {
        assert_eq!(
            suggest("apX", &["apa", "apb"]),
            vec!["apa", "apb"],
            "equal scores must not be reordered"
        );
        assert_eq!(
            jaro_winkler_similarity("apX", "apa"),
            jaro_winkler_similarity("apX", "apb"),
            "the premise: these two really do tie"
        );
        assert_eq!(
            suggest("appi", &["api", "app"]),
            vec!["app", "api"],
            "0.9417 beats 0.9333, so declaration order is overridden"
        );
    }

    /// The measured five-line body of `api create-worklog` with no options.
    #[test]
    fn a_usage_failure_puts_every_error_on_its_own_line_after_a_blank_one() {
        let body = usage_error(
            "Usage: tt-devpro api create-worklog [<options>]",
            &[
                "missing option --date".to_string(),
                "missing option --project-id".to_string(),
            ],
        );
        assert_eq!(
            body,
            "Usage: tt-devpro api create-worklog [<options>]\n\nError: missing option --date\nError: missing option --project-id"
        );
    }

    /// One message still gets the blank line; the incumbent does not special-case it.
    #[test]
    fn a_single_error_still_follows_a_blank_line() {
        assert_eq!(
            usage_error(
                "Usage: tt-devpro settle [<options>]",
                &["no such option --nosuchflag".to_string()]
            ),
            "Usage: tt-devpro settle [<options>]\n\nError: no such option --nosuchflag"
        );
    }

    /// Every rejection in `parse-date.tsv`, and the one acceptance. The shape check
    /// is what separates this from `chrono`'s `%Y-%m-%d`, which takes `2026-1-15`.
    #[test]
    fn the_date_parser_is_as_strict_as_iso_local_date() {
        assert_eq!(
            parse_iso_date("2026-01-15"),
            Ok(chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap())
        );
        assert_eq!(
            parse_iso_date("0000-01-01"),
            Ok(chrono::NaiveDate::from_ymd_opt(0, 1, 1).unwrap()),
            "year zero is a date Java accepts"
        );

        for rejected in [
            "2026-1-15",
            "2026-01-5",
            "26-01-15",
            "2026-01-15T00:00:00",
            "notadate",
            "",
            "+2026-01-15",
            "2026-01-15 ",
            " 2026-01-15",
            "99999-01-01",
            "+99999-01-01",
        ] {
            assert!(
                parse_iso_date(rejected).is_err(),
                "{rejected:?} must be rejected, as the JVM rejects it"
            );
        }
    }

    /// The shape can be right and the date still not exist. Both measured cases.
    #[test]
    fn a_well_shaped_date_that_is_not_on_the_calendar_is_rejected() {
        assert!(
            parse_iso_date("2026-02-30").is_err(),
            "February has 28 days in 2026"
        );
        assert!(
            parse_iso_date("2026-13-01").is_err(),
            "there is no month 13"
        );
        assert!(
            parse_iso_date("2024-02-29").is_ok(),
            "and a real leap day still parses"
        );
    }
}
