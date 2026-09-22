//! `commands/SettleCommand.kt`, ported — the orchestration, the four modes and
//! the interactive loop.
//!
//! The Kotlin command is one class that parses arguments, talks to two servers,
//! prints to two streams, reads the keyboard and does the arithmetic. Splitting
//! that is not a tidy-up: **the `[A]` branch writes worklogs to the live DevPro
//! portal, which other people read, so this run never executes it** (D4). Every
//! rule that branch depends on therefore has to be reachable by a test that does
//! no I/O, which is why the file reads as a stack of pure functions with a thin
//! `run` on top rather than as a transcription of the class.
//!
//! What the pure layer holds, and the contract each piece answers:
//!
//! - [`resolve_range`] — C22, the `--from`/`--to` defaults and the stderr note.
//! - [`find_existing`] — C13, CREATE vs UPDATE decided on `(date, projectId)`.
//! - [`build_actions`] — the tail of `prepareActions`, everything after the
//!   network.
//! - [`adjust_to_eight_hours`] — C14, and two of C27's five truncating sites.
//! - [`renormalize_after_edit`] — C15's redistribution, and two more of them.
//! - [`draft_table`] / [`under_eight_warning`] — the interactive display, C25.
//! - [`day_choice`] / [`batch_choice`] — C15's dispatch, including the two ways
//!   an unrecognised line ends the whole command.
//! - [`non_positive_hours`] — C19, the last gate in front of the write path.
//!
//! **Three JVM behaviours this file reproduces rather than approximates.**
//!
//! 1. **`Double.toInt()` truncates where `TimeNormalizer` rounds.** C27 says both
//!    are contracts and names all five sites; four of them are here
//!    (`SettleCommand.kt:604,618,771,844,850` — the fifth is `:850`'s sibling in
//!    the same expression). [`java_to_int`] is the truncating one, and
//!    [`crate::service::normalizer::round_to_quarter`] is the rounding one; a port
//!    that unifies them passes everything except the tests written to separate
//!    them.
//! 2. **`maxByOrNull` returns the *first* maximum**, where Rust's `max_by` returns
//!    the last. On a day whose two largest entries tie, that decides which one
//!    absorbs the rounding residual. [`first_max_by_hours`].
//! 3. **`===` is reference identity, and Rust has no equivalent.** `editEntry` and
//!    `deleteEntry` replace and remove by identity (`:774`, `:820`), and
//!    `adjustToEightHours` does the same at `:605,619`. Every one of them is ported
//!    by *index*, because `==` on `SettleAction` would hit every structurally
//!    identical action — two 0.5h meetings on the same project are exactly that.
//!
//! The one deliberate divergence from the incumbent is D2, and it lives in
//! [`prepare_actions`]: project ids are resolved per day rather than once for the
//! range's first day, `normalView` is fetched for every month the range spans
//! rather than once, and filler budgets come from
//! [`PeriodBudgets::calculate_if_configured`] per billing period. The plan's D2
//! section has the measurement behind each.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, Local, NaiveDate};
use clap::Args;

use crate::api::chrono::ChronoClient;
use crate::api::portal::{ApiError, TtApiClient};
use crate::commands::holidays::is_us_federal_holiday;
use crate::commands::settle_render::{
    action_label, clean_chrono_entry, entry_type, render_day_summary, task_title, title_case,
    under_eight_days, weekday_abbreviation,
};
use crate::commands::settle_window::{
    describe_not_final_days, last_settleable_day, nothing_to_settle_message, split_by_finality,
};
use crate::commands::{kotlin_double_or_null, parse_iso_date};
use crate::config::Config;
use crate::fmt::{java_dbl, java_fmt, java_fmt_width, pad_right, utf16_cmp, utf16_len};
use crate::model::{
    ActionType, CreateWorklogRequest, DayProjectAggregate, FillerEntry, NormalizedAggregate,
    SettleAction, UpdateWorklogRequest, WorklogDetail,
};
use crate::service::aggregator;
use crate::service::borrower::{self, BorrowedEntry};
use crate::service::filler::{self, ThreadFillerRandom};
use crate::service::filler_budget::PeriodBudgets;
use crate::service::normalizer::{TimeNormalizer, java_max, java_min};

/// `0.25`, spelled inline at every site in `SettleCommand.kt` rather than named.
/// It is both the quantum the command truncates to and the floor it clamps at,
/// and the incumbent uses the same literal for both.
const QUARTER_HOUR: f64 = 0.25;

/// The day every proposal is scaled to reach.
const FULL_DAY_HOURS: f64 = 8.0;

// ---------------------------------------------------------------------------
// Argument surface
// ---------------------------------------------------------------------------

/// `SettleCommand.kt:55-69`.
///
/// `--from` and `--to` are `.convert { LocalDate.parse(it) }` with no default, so
/// an unparseable value is a usage failure — the same ordering `api get-projects`
/// has, and the reason the conversion runs through clap's `value_parser` instead
/// of the command body. Neither carries a short letter; the captured help shows
/// `--from=<value>`, and `<value>` is Clikt's metavar for an unnamed conversion.
#[derive(Args, Debug, Clone, Default)]
pub struct SettleArgs {
    /// Start date (YYYY-MM-DD), defaults to the 1st of this month.
    #[arg(long = "from", value_parser = parse_iso_date)]
    pub from: Option<NaiveDate>,

    /// End date (YYYY-MM-DD), defaults to the last completed day.
    #[arg(long = "to", value_parser = parse_iso_date)]
    pub to: Option<NaiveDate>,

    /// Also settle today, whose hours are not final.
    #[arg(long = "include-today")]
    pub include_today: bool,

    /// Emit the proposed actions as JSON and exit without applying.
    #[arg(long = "json")]
    pub json: bool,

    /// Print a readable per-day summary and exit without applying.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
}

/// D6: the captured bytes of `tt-devpro settle --help`, wrapped at 79 columns with
/// no colour. `~/.cache/tt-devpro-rewrite/baseline/settle-help.out`, trailing
/// newline stripped.
pub const SETTLE_HELP: &str = r#"Usage: tt-devpro settle [<options>]

  Settle daily hours: normalize to 8h, auto-fill gaps, push to DevPro

Options:
  --from=<value>   Start date (YYYY-MM-DD), defaults to the 1st of this month.
                   Without --from/--to runs in day-by-day mode
  --to=<value>     End date (YYYY-MM-DD), defaults to the last completed day.
                   Without --from/--to runs in day-by-day mode
  --include-today  Also settle today. Off by default: today is unfinished, so
                   its hours aren't final
  --json           Output proposed actions as JSON and exit without applying
  --dry-run        Print a readable per-day summary of proposed actions and
                   exit without applying (--json wins if both are given)
  -h, --help       Show this message and exit"#;

// ---------------------------------------------------------------------------
// The terminal, behind a seam
// ---------------------------------------------------------------------------

/// Everything the command needs from the terminal.
///
/// [`present`](Console::present) is `System.console() != null`, which is **not**
/// "stdout is a TTY": C7 measured it on a GraalVM native image under four stdio
/// combinations and it is null unless stdin *and* stdout are both terminals. The
/// row that catches a careless port is stdin redirected with stdout on a real
/// terminal — the incumbent prints the summary there, and a port testing only
/// stdout prompts and then hits EOF.
pub trait Console {
    /// Clikt's `echo(msg)` — stdout, one trailing newline.
    fn out(&mut self, line: &str);
    /// Clikt's `echo(msg, err = true)`.
    fn err(&mut self, line: &str);
    /// Kotlin's `readLine()`. `None` is EOF, and EOF is never an empty string.
    fn read_line(&mut self) -> Option<String>;
    /// `System.console() != null`.
    fn present(&self) -> bool;

    /// `echo(msg, err = quiet)` — C7's first routing regime, where the *same* line
    /// goes to stderr under `--json`/`--dry-run` and to stdout in an interactive
    /// run. Hardcoding `eprintln!` for these four progress lines is correct on the
    /// two quiet paths and wrong on the third.
    fn echo(&mut self, line: &str, err: bool) {
        if err { self.err(line) } else { self.out(line) }
    }

    /// `echo()` with no argument: a bare newline on stdout.
    fn blank(&mut self) {
        self.out("");
    }
}

/// The production [`Console`].
pub struct Stdio {
    present: bool,
}

impl Stdio {
    pub fn new() -> Self {
        Self {
            present: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        }
    }
}

impl Default for Stdio {
    fn default() -> Self {
        Self::new()
    }
}

impl Console for Stdio {
    fn out(&mut self, line: &str) {
        println!("{line}");
    }

    fn err(&mut self, line: &str) {
        eprintln!("{line}");
    }

    fn read_line(&mut self) -> Option<String> {
        // Kotlin's `readLine()` returns null at EOF and strips the terminator;
        // every caller trims immediately afterwards, so a stray `\r` from a CRLF
        // pipe is gone either way.
        let mut buffer = String::new();
        std::io::stdout().flush().ok();
        match std::io::stdin().read_line(&mut buffer) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(
                buffer
                    .trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_string(),
            ),
        }
    }

    fn present(&self) -> bool {
        self.present
    }
}

// ---------------------------------------------------------------------------
// Kotlin's string-to-number conversions
// ---------------------------------------------------------------------------

/// Kotlin's `String.trim()`, which is `Character.isWhitespace` and **not** Rust's
/// `str::trim`.
///
/// The two disagree in both directions, measured over all of Unicode on GraalVM
/// JDK 21 (`~/.cache/tt-devpro-rewrite/measurements/kotlin/charprobe21.out`, 25
/// code points): Java counts U+001C..U+001F, the four information separators,
/// which Rust's `White_Space` property does not; Rust counts U+00A0, U+2007 and
/// U+202F, the three non-breaking spaces, which Java does not. So a line pasted
/// with a no-break space before the `a` cancels the day under the incumbent and
/// would approve it under a port that reached for `str::trim`.
pub fn kotlin_trim(s: &str) -> &str {
    s.trim_matches(is_java_whitespace)
}

/// The 25 code points `Character.isWhitespace` accepts, transcribed from the probe
/// above rather than from memory.
fn is_java_whitespace(c: char) -> bool {
    matches!(c,
        '\u{0009}'..='\u{000D}'
        | '\u{001C}'..='\u{0020}'
        | '\u{1680}'
        | '\u{2000}'..='\u{2006}'
        | '\u{2008}'..='\u{200A}'
        | '\u{2028}'
        | '\u{2029}'
        | '\u{205F}'
        | '\u{3000}'
    )
}

/// Kotlin's `String.toIntOrNull()` — `SettleCommand.kt:754,811`, the entry-number
/// prompts.
///
/// Two things the obvious `str::parse::<i32>()` gets wrong. It accepts no sign
/// forms Kotlin rejects and vice versa (both take a leading `+` or `-`, so that
/// one agrees), but Kotlin reads digits through `Character.digit`, which accepts
/// **every Unicode decimal digit** — `٣` is 3 and `１` is 1, both measured. And an
/// out-of-range value is `null` rather than an error, which the caller then folds
/// into "Invalid entry number." along with everything else.
///
/// There is no trimming here: the caller has already trimmed, and `" 3"` is
/// `null` to Kotlin.
pub fn to_int_or_null(s: &str) -> Option<i32> {
    let (negative, digits) = match s.strip_prefix(['+', '-']) {
        Some(rest) => (s.starts_with('-'), rest),
        None => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    // Kotlin accumulates negatively and checks against the negative limit, so
    // `-2147483648` parses and `2147483648` does not. `i64` reproduces that with
    // one bound check instead of two.
    let mut value: i64 = 0;
    for c in digits.chars() {
        let digit = java_digit(c)? as i64;
        value = value * 10 - digit;
        if value < i64::from(i32::MIN) {
            return None;
        }
    }
    let value = if negative { value } else { -value };
    if value > i64::from(i32::MAX) {
        return None;
    }
    Some(value as i32)
}

/// The starting code point of every contiguous run of ten Unicode decimal digits,
/// measured on GraalVM JDK 21.0.11 — the toolchain the incumbent was built with —
/// by walking all 1 114 112 code points and asking `Character.digit(c, 10)`.
/// 68 runs, 680 digits, every run contiguous. JDK 19 has 66 of them, which is why
/// the table is pinned to the JDK that ships the binary rather than to whatever
/// `java` is first on `PATH`.
/// Probe: `~/.cache/tt-devpro-rewrite/measurements/kotlin/CharProbe.java`.
const DIGIT_ZEROS: [u32; 68] = [
    0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6,
    0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80,
    0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0,
    0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0,
    0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60,
    0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0,
    0x1E950, 0x1FBF0,
];

/// `Character.digit(c, 10)`.
fn java_digit(c: char) -> Option<u32> {
    let code = c as u32;
    DIGIT_ZEROS
        .iter()
        .find(|&&zero| code >= zero && code < zero + 10)
        .map(|&zero| code - zero)
}

/// `Double.toInt()` — a narrowing conversion to **32** bits, truncating toward
/// zero, saturating at the `Int` bounds, with `NaN` becoming `0`.
///
/// Rust's `as i32` has had exactly these semantics since 1.45, so the cast alone
/// would do. It is spelled out because the thing that matters is the width: the
/// operand is `hours / 0.25`, and reading `.toInt()` as `i64` costs nothing on any
/// value this tool sees and is wrong by 2^32 on one it never will.
pub fn java_to_int(x: f64) -> i32 {
    x as i32
}

/// `((x / 0.25).toInt() * 0.25).coerceAtLeast(0.25)` — `SettleCommand.kt:604,618`.
///
/// `coerceAtLeast` is `if (this < minimumValue) minimumValue else this` on the
/// primitive, not `Math.max`, which is why this is not [`java_max`]: the two agree
/// on every value here and disagree on `NaN`, and writing the one the source
/// writes costs nothing.
fn truncate_to_quarter_at_least_quarter(x: f64) -> f64 {
    let quantized = f64::from(java_to_int(x / QUARTER_HOUR)) * QUARTER_HOUR;
    if quantized < QUARTER_HOUR {
        QUARTER_HOUR
    } else {
        quantized
    }
}

/// `maxOf(0.25, (x / 0.25).toInt() * 0.25)` — `SettleCommand.kt:844,850`.
///
/// The same arithmetic as [`truncate_to_quarter_at_least_quarter`] with the floor
/// applied through Kotlin's `maxOf`, which *is* `Math.max`. Two spellings of one
/// idea, four lines apart in the incumbent; collapsing them into one is the tidy-up
/// C27 exists to prevent, so both are here.
fn truncate_to_quarter_max_quarter(x: f64) -> f64 {
    java_max(QUARTER_HOUR, f64::from(java_to_int(x / QUARTER_HOUR)) * QUARTER_HOUR)
}

// ---------------------------------------------------------------------------
// C22 — the explicit range
// ---------------------------------------------------------------------------

/// `SettleCommand.kt:128-149`'s `ResolvedRange`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRange {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// C22. Fills in the ends the user did not type, under the rule "any date you
/// didn't type is a completed date", and returns the stderr note when the
/// resolved upper end reaches past the cutoff.
///
/// The comparison is against `cutoff`, **not** `today`: under `--include-today`
/// the cutoff *is* today, and a note saying the range runs "past the last
/// completed day" about the very day that flag just made settleable would
/// contradict itself.
///
/// The note is returned rather than printed so the two call sites
/// (`SettleCommand.kt:98`, `:287`) can stay the only place a stream is chosen.
pub fn resolve_range(
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    today: NaiveDate,
    cutoff: NaiveDate,
) -> (ResolvedRange, Option<String>) {
    let range = ResolvedRange {
        // `LocalDate.now().withDayOfMonth(1)`. Day 1 exists in every month, so the
        // `expect` is unreachable for any date `chrono` can represent.
        from: from.unwrap_or_else(|| {
            today
                .with_day(1)
                .expect("every month has a first day")
        }),
        to: to.unwrap_or(cutoff),
    };
    let note = if range.to > cutoff {
        Some(format!(
            "\u{2139} Range ends {}, past the last completed day ({cutoff}) \u{2014} those days' hours aren't final.",
            range.to
        ))
    } else {
        None
    };
    (range, note)
}

// ---------------------------------------------------------------------------
// C13 — CREATE vs UPDATE
// ---------------------------------------------------------------------------

/// C13, `SettleCommand.kt:625-633`.
///
/// Matched on `(date, projectUniqueId)` and **not** on the task title, so a day
/// already holding one worklog for a project turns the proposal into an UPDATE of
/// that worklog whatever it is called. The first match wins, in the order the
/// portal listed them.
pub fn find_existing<'a>(
    date: NaiveDate,
    project_id: &str,
    existing: &'a [(WorklogDetail, NaiveDate)],
) -> Option<&'a WorklogDetail> {
    existing
        .iter()
        .find(|(worklog, worklog_date)| {
            *worklog_date == date && worklog.project_unique_id == project_id
        })
        .map(|(worklog, _)| worklog)
}

// ---------------------------------------------------------------------------
// C14 — the final adjustment to exactly 8h
// ---------------------------------------------------------------------------

/// `actions.groupBy { it.aggregate.date }` — a `LinkedHashMap`, so dates come out
/// in first-encounter order and each group holds its actions in input order (C28).
///
/// `settle_render`'s `group_by_date` is a `BTreeMap` and is *not* interchangeable
/// with this: `adjustToEightHours` ends in `.values.flatten()`, so sorted keys
/// there would silently reorder the output of a function whose result is then
/// stably sorted — and a stable sort keeps whatever order it was handed.
fn group_by_date_in_encounter_order(actions: &[SettleAction]) -> Vec<(NaiveDate, Vec<usize>)> {
    let mut groups: Vec<(NaiveDate, Vec<usize>)> = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        match groups.iter_mut().find(|(date, _)| *date == action.aggregate.date) {
            Some((_, bucket)) => bucket.push(index),
            None => groups.push((action.aggregate.date, vec![index])),
        }
    }
    groups
}

/// `maxByOrNull { it.normalizedHours }` over a subset, returning the index of the
/// **first** maximum.
///
/// Kotlin keeps the running maximum and replaces it only on a strict `<`, so a tie
/// leaves the earlier element in place. Rust's `Iterator::max_by` returns the
/// *last* maximum, which is the opposite rule and is invisible until two entries
/// on one day carry the same hours — which is what two 0.5h meetings are.
fn first_max_by_hours(actions: &[SettleAction], candidates: &[usize]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for &index in candidates {
        match best {
            None => best = Some(index),
            Some(current) if actions[current].normalized_hours < actions[index].normalized_hours => {
                best = Some(index)
            }
            Some(_) => {}
        }
    }
    best
}

/// C14, `SettleCommand.kt:577-623`. Runs *after* fillers and borrowing are merged
/// in, and nudges one entry per day so the day totals exactly 8h.
///
/// The order of preference is the contract: a non-synthetic scalable entry is
/// adjusted if there is one, and only if there is not does a filler or borrowed
/// entry move — and then only within what is left of `max_synthetic_hours`, and
/// not at all when the cap is already spent and the day is short. Which is to say
/// the cap wins over the 8h target, which is what the under-8h warning exists to
/// report.
pub fn adjust_to_eight_hours(actions: &[SettleAction], max_synthetic_hours: f64) -> Vec<SettleAction> {
    let mut out: Vec<SettleAction> = Vec::with_capacity(actions.len());
    for (_, day) in group_by_date_in_encounter_order(actions) {
        out.extend(adjust_day(actions, &day, max_synthetic_hours));
    }
    out
}

fn adjust_day(actions: &[SettleAction], day: &[usize], max_synthetic_hours: f64) -> Vec<SettleAction> {
    let unchanged = || day.iter().map(|&i| actions[i].clone()).collect::<Vec<_>>();
    let replace = |index: usize, hours: f64| {
        day.iter()
            .map(|&i| {
                let mut action = actions[i].clone();
                if i == index {
                    action.normalized_hours = hours;
                }
                action
            })
            .collect::<Vec<_>>()
    };

    // `sumOf` is a left fold from 0.0, and `f64` addition is not associative, so
    // the fold order is part of the answer.
    let total: f64 = day.iter().map(|&i| actions[i].normalized_hours).sum();
    let diff = FULL_DAY_HOURS - total;
    if diff.abs() < 0.01 {
        return unchanged();
    }

    let synthetic_hours: f64 = day
        .iter()
        .filter(|&&i| actions[i].is_filler || actions[i].is_borrowed)
        .map(|&i| actions[i].normalized_hours)
        .sum();

    let scalable: Vec<usize> = day
        .iter()
        .copied()
        .filter(|&i| !actions[i].is_meeting && !actions[i].is_manually_fixed)
        .collect();
    if scalable.is_empty() {
        return unchanged();
    }

    let non_synthetic: Vec<usize> = scalable
        .iter()
        .copied()
        .filter(|&i| !actions[i].is_filler && !actions[i].is_borrowed)
        .collect();
    let synthetic: Vec<usize> = scalable
        .iter()
        .copied()
        .filter(|&i| actions[i].is_filler || actions[i].is_borrowed)
        .collect();

    if !non_synthetic.is_empty() {
        // The `?: return@mapValues dayActions` on `:603` cannot fire — the list is
        // known non-empty here — but the Kotlin writes it, so the `else` arm below
        // exists rather than an `expect`.
        let Some(largest) = first_max_by_hours(actions, &non_synthetic) else {
            return unchanged();
        };
        return replace(
            largest,
            truncate_to_quarter_at_least_quarter(actions[largest].normalized_hours + diff),
        );
    }

    let remaining_budget = max_synthetic_hours - synthetic_hours;
    if diff > 0.0 && remaining_budget <= 0.01 {
        // The cap is spent and the day is short: leave it short and let
        // `under_eight_warning` say so.
        return unchanged();
    }

    let Some(largest) = first_max_by_hours(actions, &synthetic) else {
        return unchanged();
    };
    // `kotlin.math.min` is `Math.min`, whose NaN and signed-zero rules are not
    // `f64::min`'s. See `java_min`'s own doc comment for the measurement.
    let adjust_amount = if diff > 0.0 {
        java_min(diff, remaining_budget)
    } else {
        diff
    };
    replace(
        largest,
        truncate_to_quarter_at_least_quarter(actions[largest].normalized_hours + adjust_amount),
    )
}

// ---------------------------------------------------------------------------
// C15 — redistribution after an edit or a delete
// ---------------------------------------------------------------------------

/// `SettleCommand.kt:829-856`.
///
/// Meetings and manually-fixed entries hold their hours; everything else is scaled
/// to fill what is left of the 8h, truncated to a quarter, floored at one quarter,
/// and then the largest of them absorbs a residual of 0.125h or more.
///
/// **The `targetHours <= 0` branch is not a rounding case.** When meetings plus
/// fixed entries already reach 8h there is nothing left to scale into, and every
/// scalable entry is set to a flat 0.25 — no scaling at all, and the day ends over
/// 8h. That is the same idea as the normalizer's negative-target path, written a
/// second time.
///
/// **The last two lines are the trap.** `associateBy { it.aggregate }` keys the
/// scaled entries by the *aggregate*, which is a value, and then every action
/// whose aggregate compares equal is replaced by the last one that claimed the
/// key. Two actions built from the same aggregate — which is what a day with two
/// identical meetings produces — therefore collapse onto one set of hours. Ported
/// as written, including last-wins.
pub fn renormalize_after_edit(actions: &[SettleAction]) -> Vec<SettleAction> {
    let fixed_hours: f64 = actions
        .iter()
        .filter(|a| a.is_meeting || a.is_manually_fixed)
        .map(|a| a.normalized_hours)
        .sum();
    let scalable: Vec<&SettleAction> = actions
        .iter()
        .filter(|a| !a.is_meeting && !a.is_manually_fixed)
        .collect();

    // Nothing to scale: the caller redraws and the warning explains it.
    if scalable.is_empty() {
        return actions.to_vec();
    }

    let scalable_hours: f64 = scalable.iter().map(|a| a.normalized_hours).sum();
    let target_hours = FULL_DAY_HOURS - fixed_hours;
    if target_hours <= 0.0 {
        return actions
            .iter()
            .map(|a| {
                let mut action = a.clone();
                if !a.is_meeting && !a.is_manually_fixed {
                    action.normalized_hours = QUARTER_HOUR;
                }
                action
            })
            .collect();
    }

    let scale_factor = target_hours / scalable_hours;
    let mut scaled: Vec<SettleAction> = scalable
        .iter()
        .map(|a| {
            let mut action = (*a).clone();
            action.normalized_hours =
                truncate_to_quarter_max_quarter(a.normalized_hours * scale_factor);
            action
        })
        .collect();

    let diff = target_hours - scaled.iter().map(|a| a.normalized_hours).sum::<f64>();
    if diff.abs() >= 0.125 && !scaled.is_empty() {
        // `sortedByDescending` is a stable TimSort over `Double.compare`, so ties
        // keep the order they had and `first()` is the earliest of the maxima —
        // the same first-wins rule as `maxByOrNull`, reached a different way.
        scaled.sort_by(|left, right| right.normalized_hours.total_cmp(&left.normalized_hours));
        scaled[0].normalized_hours =
            truncate_to_quarter_max_quarter(scaled[0].normalized_hours + diff);
    }

    // `associateBy { it.aggregate }` builds a `LinkedHashMap`, so a repeated key is
    // last-wins. `DayProjectAggregate` carries `f64` fields and cannot be `Hash`,
    // which costs a linear scan over a list that is one day long.
    let mut by_aggregate: Vec<(&DayProjectAggregate, &SettleAction)> = Vec::new();
    for action in &scaled {
        match by_aggregate
            .iter_mut()
            .find(|(key, _)| **key == action.aggregate)
        {
            Some(slot) => slot.1 = action,
            None => by_aggregate.push((&action.aggregate, action)),
        }
    }

    actions
        .iter()
        .map(|action| {
            by_aggregate
                .iter()
                .find(|(key, _)| **key == action.aggregate)
                .map(|(_, scaled)| (*scaled).clone())
                .unwrap_or_else(|| action.clone())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The tail of prepareActions
// ---------------------------------------------------------------------------

/// The project ids in force on one day, which under D2 is a different map per day
/// rather than one map for the whole range.
pub type IdsByDay = HashMap<NaiveDate, HashMap<String, String>>;

/// `SettleCommand.kt:483-575` — everything after the last network call.
///
/// Three lists of proposals in one order that the caller then sorts: the real
/// aggregates, then the fillers, then the borrowed entries. Fillers and borrowed
/// entries get a *synthetic* `DayProjectAggregate` whose `chronoProject` is the
/// literal `[filler]` or `[borrowed]` — which is what `clean_chrono_entry` keys
/// its display marker off, so the two strings are load-bearing.
///
/// The final sort is `compareBy({ date }, { devproProjectName })`, stable, with
/// the second key compared as Java compares strings. Stability is what keeps two
/// entries on one project in the order the three lists put them.
///
/// Returns `Err` where the incumbent's `projectIdMap[...]!!` would throw: a name
/// that survived `resolve_project_ids` and then went missing from the map. That
/// cannot happen through `prepare_actions`, which resolves exactly the names it
/// then looks up; it can happen to a caller assembling the map by hand, and a
/// message beats a panic there.
pub fn build_actions(
    normalized: &[NormalizedAggregate],
    fillers: &[FillerEntry],
    borrowed: &[BorrowedEntry],
    ids_by_day: &IdsByDay,
    existing_worklogs: &[(WorklogDetail, NaiveDate)],
    max_synthetic_hours: f64,
) -> Result<Vec<SettleAction>> {
    let id_for = |date: NaiveDate, name: &str| -> Result<String> {
        ids_by_day
            .get(&date)
            .and_then(|ids| ids.get(name))
            .cloned()
            .ok_or_else(|| anyhow!("DevPro project '{name}' has no id resolved for {date}"))
    };

    let mut all: Vec<SettleAction> = Vec::new();

    for norm in normalized {
        let aggregate = &norm.original;
        let devpro_project_id = id_for(aggregate.date, &aggregate.devpro_project_name)?;
        let existing = find_existing(aggregate.date, &devpro_project_id, existing_worklogs);
        all.push(SettleAction {
            aggregate: aggregate.clone(),
            normalized_hours: norm.normalized_hours,
            is_meeting: norm.is_meeting,
            is_filler: false,
            is_borrowed: false,
            source_date: None,
            // C12 at its primary site. The same derivation the borrower applies to
            // its own titles, which is why it lives in `settle_render`.
            task_title: task_title(&aggregate.descriptions, &aggregate.chrono_project),
            devpro_project_id,
            action: if existing.is_some() {
                ActionType::Update
            } else {
                ActionType::Create
            },
            existing_worklog_id: existing.map(|w| w.unique_id.clone()),
            is_manually_fixed: false,
        });
    }

    for entry in fillers {
        let devpro_project_id = id_for(entry.date, &entry.devpro_project_name)?;
        let existing = find_existing(entry.date, &devpro_project_id, existing_worklogs);
        all.push(SettleAction {
            aggregate: DayProjectAggregate {
                date: entry.date,
                chrono_project: "[filler]".to_string(),
                total_hours: entry.hours,
                descriptions: vec![entry.task_title.clone()],
                devpro_project_name: entry.devpro_project_name.clone(),
                billability: entry.billability.clone(),
                max_hours: None,
            },
            normalized_hours: entry.hours,
            is_meeting: false,
            is_filler: true,
            is_borrowed: false,
            source_date: None,
            task_title: entry.task_title.clone(),
            devpro_project_id,
            action: if existing.is_some() {
                ActionType::Update
            } else {
                ActionType::Create
            },
            existing_worklog_id: existing.map(|w| w.unique_id.clone()),
            is_manually_fixed: false,
        });
    }

    for entry in borrowed {
        let devpro_project_id = id_for(entry.date, &entry.devpro_project_name)?;
        let existing = find_existing(entry.date, &devpro_project_id, existing_worklogs);
        all.push(SettleAction {
            aggregate: DayProjectAggregate {
                date: entry.date,
                chrono_project: "[borrowed]".to_string(),
                total_hours: entry.hours,
                descriptions: vec![entry.task_title.clone()],
                devpro_project_name: entry.devpro_project_name.clone(),
                billability: entry.billability.clone(),
                max_hours: None,
            },
            normalized_hours: entry.hours,
            is_meeting: false,
            is_filler: false,
            is_borrowed: true,
            source_date: Some(entry.source_date),
            task_title: entry.task_title.clone(),
            devpro_project_id,
            action: if existing.is_some() {
                ActionType::Update
            } else {
                ActionType::Create
            },
            existing_worklog_id: existing.map(|w| w.unique_id.clone()),
            is_manually_fixed: false,
        });
    }

    let mut adjusted = adjust_to_eight_hours(&all, max_synthetic_hours);
    adjusted.sort_by(|left, right| {
        left.aggregate.date.cmp(&right.aggregate.date).then_with(|| {
            utf16_cmp(
                &left.aggregate.devpro_project_name,
                &right.aggregate.devpro_project_name,
            )
        })
    });
    Ok(adjusted)
}

// ---------------------------------------------------------------------------
// The interactive display
// ---------------------------------------------------------------------------

/// `showDraftTable` (`SettleCommand.kt:635-715`), returned as one block instead of
/// echoed line by line.
///
/// Every width here is a `String.length`, i.e. UTF-16 code units, and so is the
/// `take(w - 1)` that truncates an over-long cell. The widths are computed from the
/// **untruncated** text, so a 60-character entry both sets the column to its 50-unit
/// cap and is then cut to 49 units plus `…`.
///
/// C25 lives in the hours column: `%5.2f` of `originalHours`, which is the raw
/// Chrono total and is *not* quantized to a quarter, is exactly the case where
/// Java's HALF_UP-on-the-shortest-representation and Rust's `{:.2}` disagree.
/// Both hour figures go through [`java_fmt_width`].
pub fn draft_table(actions: &[SettleAction]) -> String {
    let date_width = 10;
    let chrono_project_width = max_width(actions, 14, |a| a.aggregate.chrono_project.clone());
    let chrono_entry_width = max_width(actions, 12, clean_chrono_entry).min(50);
    let devpro_project_width = max_width(actions, 14, |a| a.aggregate.devpro_project_name.clone());
    let task_title_width = max_width(actions, 11, |a| a.task_title.clone());
    let type_width = 8;
    let hours_width = 13;

    let header = format!(
        "{} | {} | {} | {} | {} | {} | {} | {}",
        pad_right("Date", date_width),
        pad_right("Chrono Project", chrono_project_width),
        pad_right("Chrono Entry", chrono_entry_width),
        pad_right("DevPro Project", devpro_project_width),
        pad_right("DevPro Task", task_title_width),
        pad_right("Type", type_width),
        pad_right("Hours", hours_width),
        "Action",
    );
    let separator = "-".repeat(utf16_len(&header));

    let mut lines = vec![header, separator.clone()];
    for action in actions {
        let entry = truncate_with_ellipsis(&clean_chrono_entry(action), chrono_entry_width);
        let task = truncate_with_ellipsis(&action.task_title, task_title_width);
        let original = action.aggregate.total_hours;
        let normalized = action.normalized_hours;
        let hours = if (original - normalized).abs() < 0.01 {
            java_fmt_width(normalized, 2, 5)
        } else {
            format!(
                "{}\u{2192}{}",
                java_fmt_width(original, 2, 5),
                java_fmt_width(normalized, 2, 5)
            )
        };
        lines.push(format!(
            "{} | {} | {} | {} | {} | {} | {} | {}",
            pad_right(&action.aggregate.date.to_string(), date_width),
            pad_right(&action.aggregate.chrono_project, chrono_project_width),
            pad_right(&entry, chrono_entry_width),
            pad_right(&action.aggregate.devpro_project_name, devpro_project_width),
            pad_right(&task, task_title_width),
            pad_right(entry_type(action), type_width),
            pad_right(&hours, hours_width),
            action_label(action.action),
        ));
    }

    let original_total: f64 = actions.iter().map(|a| a.aggregate.total_hours).sum();
    let normalized_total: f64 = actions.iter().map(|a| a.normalized_hours).sum();
    lines.push(separator);
    lines.push(format!(
        "Total: {} \u{2192} {} hours, {} entries",
        java_fmt(original_total, 2),
        java_fmt(normalized_total, 2),
        actions.len()
    ));
    lines.join("\n")
}

/// `maxOf(floor, rows.maxOfOrNull { it.<column>.length } ?: floor)`.
///
/// The `?: floor` arm is reachable only with no rows at all, which every caller
/// rules out before calling — ported anyway, because the empty table it produces
/// is a header of minimum widths rather than a panic.
fn max_width(
    actions: &[SettleAction],
    floor: usize,
    cell: impl Fn(&SettleAction) -> String,
) -> usize {
    actions
        .iter()
        .map(|a| utf16_len(&cell(a)))
        .max()
        .unwrap_or(floor)
        .max(floor)
}

/// `if (s.length > w) s.take(w - 1) + "…" else s` — `SettleCommand.kt:680-684`.
///
/// `take` counts UTF-16 code units and a surrogate pair is two of them, so cutting
/// at `w - 1` can land between the halves of an astral character. Kotlin's `take`
/// is `substring(0, n)`, which happily returns a lone high surrogate; Rust cannot
/// hold one in a `String`, so a cut that would split a pair keeps the whole
/// character and the cell is one unit wider than the incumbent's. The alternative
/// is `String::from_utf16_lossy`, which turns it into U+FFFD — a visible corruption
/// in place of a one-column overhang. Named here because the data that reaches it
/// is Chrono descriptions and an emoji in one is not exotic.
fn truncate_with_ellipsis(text: &str, width: usize) -> String {
    if utf16_len(text) <= width {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut units = 0;
    for c in text.chars() {
        let size = c.len_utf16();
        if units + size > width - 1 {
            break;
        }
        kept.push(c);
        units += size;
    }
    kept.push('\u{2026}');
    kept
}

/// `showUnderEightWarning` (`SettleCommand.kt:717-727`), returning the block when
/// there is one and `None` when there is not — which is the `Boolean` the incumbent
/// returns, carried by the same value that holds the text.
///
/// The glyph is U+26A0 U+FE0F followed by **two** spaces. The other warning in this
/// file, the `project_ids` fallback, is a bare U+26A0 and one space. They are
/// different strings and normalising them to one breaks byte parity against the
/// captures.
pub fn under_eight_warning(actions: &[SettleAction]) -> Option<String> {
    let under = under_eight_days(actions);
    if under.is_empty() {
        return None;
    }
    let mut lines = vec![
        String::new(),
        "\u{26A0}\u{FE0F}  WARNING: Some days don't reach 8h due to borrowed+filler cap:"
            .to_string(),
    ];
    for (date, hours) in under {
        lines.push(format!(
            "  {date}: {}h (need {}h more)",
            java_fmt(hours, 2),
            java_fmt(FULL_DAY_HOURS - hours, 2)
        ));
    }
    Some(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// C15 — the dispatch
// ---------------------------------------------------------------------------

/// What one line typed at the day-by-day prompt means.
///
/// The two ways out are separate on purpose: `c` and EOF print `Cancelled.`, while
/// **anything else — including a bare Enter** — prints `Unknown option. Cancelled.`
/// Both abandon every remaining day. A port that re-prompts on a typo is friendlier
/// and is not this program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayChoice {
    Approve,
    Edit,
    Delete,
    Skip,
    Cancel,
    Unknown,
}

/// `SettleCommand.kt:378-403`. The input has already been through
/// `readLine()?.trim()?.lowercase()`; `None` is EOF.
pub fn day_choice(input: Option<&str>) -> DayChoice {
    match input {
        Some("a") => DayChoice::Approve,
        Some("e") => DayChoice::Edit,
        Some("d") => DayChoice::Delete,
        Some("s") => DayChoice::Skip,
        Some("c") | None => DayChoice::Cancel,
        Some(_) => DayChoice::Unknown,
    }
}

/// The batch prompt's three answers. `runBatchMode` has no loop, no edit, no
/// delete and no skip — one question, and all three answers leave the function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchChoice {
    Approve,
    Cancel,
    Unknown,
}

/// `SettleCommand.kt:176-180`.
pub fn batch_choice(input: Option<&str>) -> BatchChoice {
    match input {
        Some("a") => BatchChoice::Approve,
        Some("c") | None => BatchChoice::Cancel,
        Some(_) => BatchChoice::Unknown,
    }
}

/// The outer prompt's text, which changes when the under-8h warning fired.
fn day_prompt(has_warning: bool) -> &'static str {
    if has_warning {
        "\n[A]pprove anyway / [E]dit / [D]elete / [S]kip / [C]ancel all: "
    } else {
        "\n[A]pprove / [E]dit / [D]elete / [S]kip / [C]ancel all: "
    }
}

/// The batch prompt's text.
fn batch_prompt(has_warning: bool) -> &'static str {
    if has_warning {
        "\n[A]pprove anyway / [C]ancel: "
    } else {
        "\n[A]pprove / [C]ancel: "
    }
}

/// Reads one line and applies `trim().lowercase()` — the outer prompts' rule.
///
/// Kotlin's `lowercase()` is `toLowerCase(Locale.ROOT)`, full Unicode, which
/// `str::to_lowercase` also is. The two can differ on a handful of code points
/// (final sigma, U+0130), none of which lowercase to `a`, `c`, `d`, `e` or `s`.
fn read_choice(io: &mut dyn Console) -> Option<String> {
    io.read_line()
        .map(|line| kotlin_trim(&line).to_lowercase())
}

/// Reads one line and applies `trim()` only — the *inner* prompts' rule.
///
/// The missing `lowercase()` is not an oversight to fix: `B` is therefore not `b`,
/// so a capital B falls through to `toIntOrNull`, fails, and prints
/// `Invalid entry number.` rather than going back.
fn read_entry(io: &mut dyn Console) -> Option<String> {
    io.read_line().map(|line| kotlin_trim(&line).to_string())
}

// ---------------------------------------------------------------------------
// C15 — edit and delete
// ---------------------------------------------------------------------------

/// One line of the `Editable entries:` / `Deletable entries:` listing.
///
/// The `*` marks a manually-fixed entry. Both listings print it; only the edit
/// listing explains it afterwards.
fn entry_listing_line(position: usize, action: &SettleAction) -> String {
    let marker = if action.is_manually_fixed { "*" } else { " " };
    format!(
        "  {}.{marker} {}: {} ({}h)",
        position + 1,
        action.aggregate.devpro_project_name,
        action.task_title,
        java_fmt(action.normalized_hours, 2)
    )
}

/// Resolves what the operator typed at an `Entry number` prompt against a listing
/// of `count` entries.
///
/// `None` from the console is EOF, and EOF here means "go back" — the opposite of
/// what the same empty line means at the outer prompt, and both are contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrySelection {
    /// `b`, an empty line, or EOF.
    GoBack,
    /// Not a number, or a number outside `1..=count`.
    Invalid,
    /// A zero-based index into the listing.
    Chosen(usize),
}

/// `SettleCommand.kt:752-760` and the identical `:809-817`.
///
/// `toIntOrNull()?.minus(1)` is Kotlin `Int` arithmetic, which wraps: the input
/// `-2147483648` becomes `2147483647` rather than overflowing, and is then rejected
/// for being out of range. `wrapping_sub` is that, and `checked_sub` would be a
/// different program.
pub fn entry_selection(input: Option<&str>, count: usize) -> EntrySelection {
    let Some(text) = input else {
        return EntrySelection::GoBack;
    };
    if text == "b" || text.is_empty() {
        return EntrySelection::GoBack;
    }
    let Some(number) = to_int_or_null(text) else {
        return EntrySelection::Invalid;
    };
    let index = number.wrapping_sub(1);
    if index < 0 || i64::from(index) >= count as i64 {
        return EntrySelection::Invalid;
    }
    EntrySelection::Chosen(index as usize)
}

/// `editEntry` (`SettleCommand.kt:729-786`).
///
/// **The two gates count different sets, and that is deliberate.** The listing is
/// every non-meeting entry; the "need at least 2" gate counts only entries that are
/// neither a meeting nor manually fixed, because the redistribution needs one left
/// that can still absorb it. So a day with one meeting, one fixed entry and one
/// scalable entry refuses `[E]` and allows `[D]`. Collapsing the two sets is a
/// one-character change that also shifts every index the operator types.
///
/// Returns the list unchanged on every refusal, including the 8h floor at the end —
/// which `deleteEntry` does not have.
pub fn edit_entry(actions: &[SettleAction], io: &mut dyn Console) -> Vec<SettleAction> {
    let editable: Vec<usize> = (0..actions.len())
        .filter(|&i| !actions[i].is_meeting)
        .collect();
    if editable.is_empty() {
        io.out("No editable entries (meetings cannot be edited).");
        return actions.to_vec();
    }

    let scalable = actions
        .iter()
        .filter(|a| !a.is_meeting && !a.is_manually_fixed)
        .count();
    if scalable < 2 {
        io.out("\u{2717} Cannot edit: need at least 2 work entries to redistribute hours.");
        return actions.to_vec();
    }

    io.out("\nEditable entries:");
    for (position, &index) in editable.iter().enumerate() {
        let line = entry_listing_line(position, &actions[index]);
        io.out(&line);
    }
    io.out("  (* = manually fixed, won't scale)");

    io.out("\nEntry number (or 'b' to go back): ");
    let typed = read_entry(io);
    let position = match entry_selection(typed.as_deref(), editable.len()) {
        EntrySelection::GoBack => return actions.to_vec(),
        EntrySelection::Invalid => {
            io.out("Invalid entry number.");
            return actions.to_vec();
        }
        EntrySelection::Chosen(position) => position,
    };
    let selected = editable[position];

    io.out(&format!(
        "Current: {}h. New hours: ",
        java_fmt(actions[selected].normalized_hours, 2)
    ));
    let typed = read_entry(io);
    // EOF and an empty line are both "leave it alone"; a value that will not parse
    // is a refusal with a message.
    let Some(hours_input) = typed.filter(|text| !text.is_empty()) else {
        return actions.to_vec();
    };
    let new_hours = kotlin_double_or_null(&hours_input);
    let Some(new_hours) = new_hours.filter(|hours| *hours >= QUARTER_HOUR) else {
        io.out("Invalid. Must be >= 0.25");
        return actions.to_vec();
    };

    // No floor here, unlike the two adjustment sites: the `>= 0.25` test above has
    // already refused anything that would truncate to zero.
    let rounded_hours = f64::from(java_to_int(new_hours / QUARTER_HOUR)) * QUARTER_HOUR;

    let mut edited = actions.to_vec();
    edited[selected].normalized_hours = rounded_hours;
    edited[selected].is_manually_fixed = true;
    let result = renormalize_after_edit(&edited);

    let total_hours: f64 = result.iter().map(|a| a.normalized_hours).sum();
    if total_hours < 7.99 {
        io.out(&format!(
            "\u{2717} Cannot set {}h \u{2014} would result in {}h total (< 8h)",
            java_dbl(rounded_hours),
            java_fmt(total_hours, 2)
        ));
        io.out(&format!(
            "  Minimum for this entry: {}h",
            java_fmt(rounded_hours + (FULL_DAY_HOURS - total_hours), 2)
        ));
        return actions.to_vec();
    }

    result
}

/// `deleteEntry` (`SettleCommand.kt:788-827`).
///
/// Both the listing and the gate count the same `!isMeeting` set here, which is the
/// difference from `editEntry`. And there is no 8h floor afterwards: deleting *can*
/// drop the day below 8h, and the next redraw shows the warning and offers
/// `[A]pprove anyway`. That is coherent, not a bug, and must not be made consistent.
pub fn delete_entry(actions: &[SettleAction], io: &mut dyn Console) -> Vec<SettleAction> {
    let deletable: Vec<usize> = (0..actions.len())
        .filter(|&i| !actions[i].is_meeting)
        .collect();
    if deletable.is_empty() {
        io.out("No deletable entries (meetings cannot be deleted).");
        return actions.to_vec();
    }
    if deletable.len() < 2 {
        io.out("\u{2717} Cannot delete: need at least 2 work entries.");
        return actions.to_vec();
    }

    io.out("\nDeletable entries:");
    for (position, &index) in deletable.iter().enumerate() {
        let line = entry_listing_line(position, &actions[index]);
        io.out(&line);
    }

    io.out("\nEntry number to delete (or 'b' to go back): ");
    let typed = read_entry(io);
    let position = match entry_selection(typed.as_deref(), deletable.len()) {
        EntrySelection::GoBack => return actions.to_vec(),
        EntrySelection::Invalid => {
            io.out("Invalid entry number.");
            return actions.to_vec();
        }
        EntrySelection::Chosen(position) => position,
    };
    let removed = deletable[position];

    let remaining: Vec<SettleAction> = actions
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != removed)
        .map(|(_, action)| action.clone())
        .collect();
    let result = renormalize_after_edit(&remaining);

    io.out(&format!(
        "\u{2713} Deleted: {}",
        actions[removed].task_title
    ));
    result
}

// ---------------------------------------------------------------------------
// C19 / C18 — the write path
// ---------------------------------------------------------------------------

/// C19's pre-flight filter, `SettleCommand.kt:860-861`.
///
/// A `SKIP` action is exempt however few hours it carries, because it is never
/// sent. Everything else must be strictly positive or the whole batch aborts
/// before the first POST — which is the point: a partial write is worse than none.
pub fn non_positive_hours(actions: &[SettleAction]) -> Vec<&SettleAction> {
    actions
        .iter()
        .filter(|a| a.normalized_hours <= 0.0 && a.action != ActionType::Skip)
        .collect()
}

/// `SettleCommand.kt:876-883`. `expenseType` is the literal `"None"` on this path;
/// every other optional field is absent, which C18 pins as an explicit `null` in the
/// body.
pub fn create_request(action: &SettleAction) -> CreateWorklogRequest {
    CreateWorklogRequest {
        worklog_date: action.aggregate.date.to_string(),
        project_unique_id: action.devpro_project_id.clone(),
        task_title: action.task_title.clone(),
        billability: action.aggregate.billability.clone(),
        duration: action.normalized_hours,
        description: None,
        overtime: None,
        expense_type: Some("None".to_string()),
        pif: None,
        google_calendar_event_id: None,
    }
}

/// `SettleCommand.kt:889-896`.
///
/// The incumbent writes `existingWorklogId!!` here, inside the per-action `try`, so
/// an `UPDATE` with no id becomes one counted failure rather than a crash. This
/// returns `Err` for the same reason and with a message of its own — Kotlin's would
/// be a `NullPointerException` text, which is not worth reproducing for a state the
/// `CREATE`/`UPDATE` decision cannot produce.
pub fn update_request(action: &SettleAction) -> Result<UpdateWorklogRequest> {
    let unique_id = action
        .existing_worklog_id
        .clone()
        .ok_or_else(|| anyhow!("action.existingWorklogId must not be null"))?;
    Ok(UpdateWorklogRequest {
        unique_id,
        worklog_date: action.aggregate.date.to_string(),
        project_unique_id: action.devpro_project_id.clone(),
        task_title: action.task_title.clone(),
        billability: action.aggregate.billability.clone(),
        duration: action.normalized_hours,
        description: None,
        overtime: None,
        expense_type: Some("None".to_string()),
        pif: None,
    })
}

/// `applyAll` (`SettleCommand.kt:858-910`). **The only function here that writes to
/// the portal**, and the one this run never executes (D4).
///
/// Validation first, over the whole batch; then one call per action, each in its own
/// `try`, so a single rejection is counted and the rest still go. The tally line is
/// printed whatever happened.
async fn apply_all(
    actions: &[SettleAction],
    client: &TtApiClient,
    io: &mut dyn Console,
) -> Result<()> {
    let invalid = non_positive_hours(actions);
    if !invalid.is_empty() {
        let count = invalid.len();
        let lines: Vec<String> = invalid
            .iter()
            .map(|action| {
                format!(
                    "\u{2717} Invalid hours: {} {} ({}h)",
                    action.aggregate.date,
                    action.aggregate.devpro_project_name,
                    java_dbl(action.normalized_hours)
                )
            })
            .collect();
        for line in lines {
            io.err(&line);
        }
        bail!("Found {count} entries with non-positive hours. Aborting.");
    }

    let mut created = 0;
    let mut updated = 0;
    let mut errors = 0;

    for action in actions {
        let outcome: Result<Option<&'static str>> = match action.action {
            ActionType::Create => client
                .create_worklog(&create_request(action))
                .await
                .map(|_| Some("Created")),
            ActionType::Update => match update_request(action) {
                Ok(request) => client.update_worklog(&request).await.map(|_| Some("Updated")),
                Err(error) => Err(error),
            },
            ActionType::Skip => Ok(None),
        };

        match outcome {
            Ok(Some(verb)) => {
                if verb == "Created" {
                    created += 1;
                } else {
                    updated += 1;
                }
                let line = format!(
                    "\u{2713} {verb}: {} {} ({}h)",
                    action.aggregate.date,
                    action.aggregate.devpro_project_name,
                    java_dbl(action.normalized_hours)
                );
                io.out(&line);
            }
            Ok(None) => {}
            Err(error) => {
                errors += 1;
                let line = format!(
                    "\u{2717} Failed: {} {} - {error}",
                    action.aggregate.date, action.aggregate.devpro_project_name
                );
                io.err(&line);
            }
        }
    }

    io.out(&format!(
        "\nDone! Created: {created}, Updated: {updated}, Errors: {errors}"
    ));
    Ok(())
}
