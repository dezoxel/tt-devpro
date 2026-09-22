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

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, Days, Local, NaiveDate, Weekday};
use clap::Args;
use serde::Serialize;

use crate::api::chrono::ChronoClient;
use crate::api::portal::{ApiError, TtApiClient};
use crate::commands::holidays::is_us_federal_holiday;
use crate::commands::settle_render::{
    action_label, clean_chrono_entry, entry_type, render_day_summary, task_title,
    under_eight_days, weekday_abbreviation, weekday_name,
};
use crate::commands::settle_window::{
    describe_not_final_days, last_settleable_day, nothing_to_settle_message, split_by_finality,
};
use crate::commands::{Outcome, kotlin_double_or_null, parse_iso_date};
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

// ---------------------------------------------------------------------------
// The scan window
// ---------------------------------------------------------------------------

/// `SettleCommand.kt:195` — `today.minusDays(45)`. Named here because the number
/// is the whole of the scan's lower bound and the project's own CLAUDE.md quotes
/// it ("Scans the last 45 days").
const SCAN_DAYS: u64 = 45;

/// Every month the closed interval `[start, end]` touches, as the first day of
/// each, in chronological order.
///
/// `SettleCommand.kt:209-214` builds this with a `mutableSetOf` and a `while
/// (!current.isAfter(cutoff))`, so the bound is inclusive and an interval whose
/// ends sit in one month yields exactly one month. An inverted interval yields
/// none, which is the Kotlin loop's behaviour too and is why
/// [`PeriodBudgets::calculate_if_configured`] has its own empty-set fallback.
///
/// D2 adds the second caller: `prepareActions` fetched `normalView` once, for the
/// range's first day, and now fetches one per month this returns.
pub fn months_in_range(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut months = Vec::new();
    let mut current = start.with_day(1).expect("every month has a first day");
    while current <= end {
        months.push(current);
        current = next_month(current);
    }
    months
}

/// `LocalDate.plusMonths(1)` on a first-of-month date, where it cannot clamp.
fn next_month(first_of_month: NaiveDate) -> NaiveDate {
    let (year, month) = if first_of_month.month() == 12 {
        (first_of_month.year() + 1, 1)
    } else {
        (first_of_month.year(), first_of_month.month() + 1)
    };
    NaiveDate::from_ymd_opt(year, month, 1).expect("the first of the next month is a date")
}

/// `LocalDate.parse(it.date.substring(0, 10))` — `SettleCommand.kt:222,446`.
///
/// The portal's `date` is an ISO timestamp and only its date half is read. Kotlin's
/// `substring` counts UTF-16 units and throws when the string is shorter; this
/// takes the first ten `char`s and says so when there are not ten, which is the
/// same outcome with a message instead of an index in it.
fn detail_date(raw: &str) -> Result<NaiveDate> {
    let head: String = raw.chars().take(10).collect();
    if head.chars().count() < 10 {
        bail!("the portal returned '{raw}', which is too short to hold a date");
    }
    parse_iso_date(&head).map_err(|message| anyhow!("{message}"))
}

/// The result of `findUnfilledDays` (`SettleCommand.kt:184-189`).
///
/// `not_final` is carried rather than dropped so an empty `unfilled_days` can tell
/// "everything is settled" from "nothing was final enough to look at" — the two
/// have different messages and only this field separates them.
#[derive(Debug, Clone, PartialEq)]
pub struct UnfilledDays {
    pub unfilled_days: Vec<NaiveDate>,
    pub devpro_hours_by_day: HashMap<NaiveDate, f64>,
    pub not_final: Vec<NaiveDate>,
}

/// C16's filter, lifted out of the fetch so it can be tested without a portal.
///
/// A day is offered when it has Chrono data, is logged under 8h in DevPro, is not
/// a weekend and is not a US federal holiday. `devpro_hours_by_day` missing a day
/// means zero hours, which is the common case — a day nobody has touched.
///
/// The comparison is `< 8.0` on the portal's own figure, not on a rounded one: a
/// day logged at 7.99h is unfilled and a day at 8.0h is not.
pub fn unfilled_days(
    settleable: &[NaiveDate],
    devpro_hours_by_day: &HashMap<NaiveDate, f64>,
) -> Vec<NaiveDate> {
    settleable
        .iter()
        .copied()
        .filter(|day| {
            let hours = devpro_hours_by_day.get(day).copied().unwrap_or(0.0);
            let weekend = matches!(day.weekday(), Weekday::Sat | Weekday::Sun);
            hours < FULL_DAY_HOURS && !weekend && !is_us_federal_holiday(*day)
        })
        .collect()
}

/// The distinct local dates a batch of Chrono entries falls on, sorted.
///
/// C2: an entry is re-dated to the local day of its `start_time`, which is the
/// only thing that makes the `cutoff + 1 day` fetch padding safe. `distinct()`
/// then `sorted()` in Kotlin (`SettleCommand.kt:236-241`); the sort is total over
/// dates, so the first-encounter order `distinct` preserves is not observable and
/// a `Vec` with a membership test reproduces it either way.
fn chrono_days(entries: &[crate::model::ChronoTimeEntry]) -> Result<Vec<NaiveDate>> {
    let mut days: Vec<NaiveDate> = Vec::new();
    for entry in entries {
        let day = aggregator::entry_local_date(&entry.start_time, &Local)?;
        if !days.contains(&day) {
            days.push(day);
        }
    }
    days.sort();
    Ok(days)
}

// ---------------------------------------------------------------------------
// The command, with its two clients
// ---------------------------------------------------------------------------

/// Everything `SettleCommand` holds between its methods once the arguments, the
/// two clients and "today" are no longer read from global state.
///
/// `today` is a field rather than a `LocalDate.now()` at each of the four sites
/// that needed it: the incumbent reads the clock five times in one run
/// (`:77,131,133,192,313,331`) and a run crossing midnight between two of them
/// would contradict itself. Reading it once is the same behaviour every other
/// second of the day and is testable.
struct Settle<'a> {
    args: &'a SettleArgs,
    config: &'a Config,
    chrono_client: &'a ChronoClient,
    tt_client: &'a TtApiClient,
    normalizer: &'a TimeNormalizer,
    today: NaiveDate,
}

/// `collectActions`' return (`SettleCommand.kt:265-268`).
struct CollectedActions {
    actions: Vec<SettleAction>,
    not_final: Vec<NaiveDate>,
}

impl Settle<'_> {
    /// C7's routing switch (`SettleCommand.kt:73`) — `json || dryRun ||
    /// System.console() == null`. The progress lines go to stderr when it holds, so
    /// that stdout carries only the JSON or the summary.
    fn quiet(&self, io: &dyn Console) -> bool {
        self.args.json || self.args.dry_run || !io.present()
    }

    /// `settleThrough` (`SettleCommand.kt:77`).
    fn cutoff(&self) -> NaiveDate {
        last_settleable_day(self.today, self.args.include_today)
    }

    /// True when the invocation named at least one end of a range, which is the
    /// condition three separate places in the incumbent branch on.
    fn explicit_range(&self) -> bool {
        self.args.from.is_some() || self.args.to.is_some()
    }

    /// [`resolve_range`] against this run's clock, with the note echoed to stderr.
    ///
    /// The two call sites (`:98`, `:287`) are mutually exclusive per invocation, so
    /// the note still fires at most once.
    fn resolve_range(&self, io: &mut dyn Console) -> ResolvedRange {
        let (range, note) = resolve_range(self.args.from, self.args.to, self.today, self.cutoff());
        if let Some(note) = note {
            io.err(&note);
        }
        range
    }

    /// One `normalView` per month, flattened into the `(worklog, date)` pairs
    /// [`find_existing`] and the budget calculation both read.
    ///
    /// D2's second facet: `SettleCommand.kt:440-446` calls this once with the
    /// range's **first day** as the period, so for a range spanning two months the
    /// second month's worklogs are invisible, `findExisting` finds nothing and
    /// every proposal there becomes a CREATE against a day that already holds a
    /// worklog. The endpoint keys on the month and not the day — measured
    /// 2026-09-22, `~/.cache/tt-devpro-rewrite/measurements/portal/normalview-keys-on-month.md`:
    /// `period=2026-09-01`, `2026-09-18` and `2026-09-30` return byte-identical
    /// bodies — so passing the first of each month is the same request the
    /// incumbent makes on the single-month path that every baseline capture
    /// exercises.
    async fn existing_worklogs(&self, months: &[NaiveDate]) -> Result<Vec<(WorklogDetail, NaiveDate)>> {
        let mut existing = Vec::new();
        for month in months {
            let view = self.tt_client.get_normal_view(&month.to_string()).await?;
            for page in &view.page_list {
                for day in &page.details_by_dates {
                    let date = detail_date(&day.date)?;
                    for worklog in &day.worklogs_details {
                        existing.push((worklog.clone(), date));
                    }
                }
            }
        }
        Ok(existing)
    }

    /// `findUnfilledDays` (`SettleCommand.kt:191-261`) — the 45-day scan.
    async fn find_unfilled_days(&self, io: &mut dyn Console) -> Result<UnfilledDays> {
        let quiet = self.quiet(io);
        let cutoff = self.cutoff();
        let range_start = self
            .today
            .checked_sub_days(Days::new(SCAN_DAYS))
            .ok_or_else(|| anyhow!("date underflow: {SCAN_DAYS} days before {}", self.today))?;

        // `:201`. Both ends are named: "last 45 days" said nothing about the upper
        // one, which is precisely where this command used to be wrong.
        io.echo(
            &format!("Checking {range_start} to {cutoff} for unfilled days (<8h)..."),
            quiet,
        );

        // C17, `:205`. The user is not needed here; the call is made so that a dead
        // session fails before 45 days of months are fetched.
        self.tt_client.get_current_user().await?;

        let months = months_in_range(range_start, cutoff);
        let mut devpro_hours_by_day: HashMap<NaiveDate, f64> = HashMap::new();
        for month in &months {
            let view = self.tt_client.get_normal_view(&month.to_string()).await?;
            for page in &view.page_list {
                for day in &page.details_by_dates {
                    // `:223` — a plain `put`, so a date appearing in two months'
                    // responses keeps the later one.
                    devpro_hours_by_day.insert(detail_date(&day.date)?, day.logged_hours);
                }
            }
        }

        // C2's padding: `cutoff + 1 day` on the UTC axis, undone by re-dating every
        // entry to its local day in `chrono_days`.
        let fetch_end = cutoff
            .succ_opt()
            .ok_or_else(|| anyhow!("date overflow: the day after {cutoff}"))?;
        let all_entries = self
            .chrono_client
            .get_time_entries(range_start, fetch_end)
            .await?;
        if all_entries.is_empty() {
            // `:232` returns the hours it already has and an empty `notFinal`, so an
            // empty Chrono says "all settled" rather than "held back".
            return Ok(UnfilledDays {
                unfilled_days: Vec::new(),
                devpro_hours_by_day,
                not_final: Vec::new(),
            });
        }

        let days = chrono_days(&all_entries)?;
        let window = split_by_finality(&days, self.today, self.args.include_today);
        if !window.not_final.is_empty() {
            // Always stderr (`:250`), in every mode.
            io.err(&format!(
                "\u{2139} Skipped (hours not final yet): {}",
                describe_not_final_days(&window.not_final, self.today)
            ));
        }

        Ok(UnfilledDays {
            unfilled_days: unfilled_days(&window.settleable, &devpro_hours_by_day),
            devpro_hours_by_day,
            not_final: window.not_final,
        })
    }

    /// `prepareActions` (`SettleCommand.kt:411-577`) — the network half; the
    /// arithmetic is [`build_actions`].
    ///
    /// D2's first and third facets live here. Project ids are resolved **per day**
    /// rather than once for `from`, because assignments exist per date and a range
    /// spanning an assignment change dies today on the one name that is not in the
    /// first day's list. Filler budgets come from
    /// [`PeriodBudgets::calculate_if_configured`], which builds one map per billing
    /// period the range spans instead of using the first period's for all of them.
    async fn prepare_actions(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        io: &mut dyn Console,
    ) -> Result<Vec<SettleAction>> {
        let quiet = self.quiet(io);

        io.echo(&format!("Fetching Chrono data ({from} to {to})..."), quiet);
        let fetch_end = to
            .succ_opt()
            .ok_or_else(|| anyhow!("date overflow: the day after {to}"))?;
        let entries = self.chrono_client.get_time_entries(from, fetch_end).await?;
        if entries.is_empty() {
            io.echo("No entries found in Chrono for this period.", quiet);
            return Ok(Vec::new());
        }

        let raw_aggregates = aggregator::aggregate(&entries, self.config, Some(from), Some(to))?;
        if raw_aggregates.is_empty() {
            io.echo(
                "No work entries to process (entries without project or duration are skipped).",
                quiet,
            );
            return Ok(Vec::new());
        }

        let normalized = self.normalizer.normalize(&raw_aggregates);

        let months = months_in_range(from, to);
        let existing_worklogs = self.existing_worklogs(&months).await?;

        let mut period_budgets = PeriodBudgets::calculate_if_configured(
            &self.config.fillers,
            &existing_worklogs,
            from,
            to,
        );

        let fillers = filler::generate_fillers(
            &normalized,
            &self.config.fillers,
            self.config.max_synthetic_hours,
            period_budgets.as_mut(),
            &mut ThreadFillerRandom,
        );

        let borrowed = borrower::borrow_for_meeting_only_days(
            &normalized,
            &fillers,
            self.chrono_client,
            self.config,
            self.config.max_synthetic_hours,
            self.normalizer,
        )
        .await?;

        let user = self.tt_client.get_current_user().await?;
        let ids_by_day = self
            .resolve_ids_per_day(&user.unique_id, &normalized, &fillers, &borrowed, io)
            .await?;

        build_actions(
            &normalized,
            &fillers,
            &borrowed,
            &ids_by_day,
            &existing_worklogs,
            self.config.max_synthetic_hours,
        )
    }

    /// D2's first facet: `getAssignedProjects` once per day that has proposals,
    /// resolving only the names that day actually needs.
    ///
    /// `SettleCommand.kt:463-478` asks the portal for the assignments held on
    /// `from` and resolves the **union** of every name in the range against that one
    /// list. Measured: `--from 2026-08-01 --to 2026-08-15 --json` exits having
    /// printed nothing but `✗ Error: DevPro project 'Inveniam SOW #5' not found.`,
    /// because `#5` is assigned later in the range and is in neither 08-01's list
    /// nor `project_ids`. Per day, each name is asked of the list that was in force
    /// when the work happened, and the whole class of failure goes away.
    ///
    /// **The fallback warnings are deduplicated across days.** The incumbent prints
    /// one line per fallback for the whole run; resolving per day would otherwise
    /// print the same line once per day the project appears on. For a single-day
    /// range — which is every invocation the scan path makes, and every baseline
    /// capture — the two are the same lines in the same order.
    async fn resolve_ids_per_day(
        &self,
        contact_id: &str,
        normalized: &[NormalizedAggregate],
        fillers: &[FillerEntry],
        borrowed: &[BorrowedEntry],
        io: &mut dyn Console,
    ) -> Result<IdsByDay> {
        // `:470-472`'s order within a day: the real aggregates, then the fillers,
        // then the borrowed entries, each distinct.
        let mut names_by_day: Vec<(NaiveDate, Vec<String>)> = Vec::new();
        for norm in normalized {
            push_name(
                &mut names_by_day,
                norm.original.date,
                &norm.original.devpro_project_name,
            );
        }
        for entry in fillers {
            push_name(&mut names_by_day, entry.date, &entry.devpro_project_name);
        }
        for entry in borrowed {
            push_name(&mut names_by_day, entry.date, &entry.devpro_project_name);
        }

        let mut ids_by_day: IdsByDay = HashMap::new();
        let mut fallbacks: Vec<aggregator::FallbackId> = Vec::new();
        for (date, names) in &names_by_day {
            let response = self
                .tt_client
                .get_assigned_projects(contact_id, &date.to_string())
                .await?;
            let resolution =
                aggregator::resolve_project_ids(names, &response.projects, &self.config.project_ids)?;
            for fallback in resolution.fallbacks {
                if !fallbacks.contains(&fallback) {
                    fallbacks.push(fallback);
                }
            }
            ids_by_day.insert(*date, resolution.ids_by_name);
        }

        // `:475-481`. Always stderr: keeps `--json` stdout clean and stays visible in
        // an interactive run. The glyph is a bare U+26A0 with one space, unlike the
        // under-8h warning's U+26A0 U+FE0F with two.
        for fallback in &fallbacks {
            io.err(&format!(
                "\u{26A0} '{}' is not in your assigned projects \u{2014} using id {} from project_ids in ~/.tt-config.yaml. Check it still points at the right project.",
                fallback.name, fallback.id
            ));
        }

        Ok(ids_by_day)
    }

    /// `collectActions` (`SettleCommand.kt:278-295`), shared by JSON and dry-run.
    ///
    /// An explicit range is taken at face value and so reports no held-back days;
    /// the scan path reports what the finality split dropped.
    async fn collect_actions(&self, io: &mut dyn Console) -> Result<CollectedActions> {
        if self.explicit_range() {
            let range = self.resolve_range(io);
            return Ok(CollectedActions {
                actions: self.prepare_actions(range.from, range.to, io).await?,
                not_final: Vec::new(),
            });
        }

        let scan = self.find_unfilled_days(io).await?;
        let mut actions = Vec::new();
        for day in &scan.unfilled_days {
            actions.extend(self.prepare_actions(*day, *day, io).await?);
        }
        Ok(CollectedActions {
            actions,
            not_final: scan.not_final,
        })
    }

    /// `runJsonMode` (`SettleCommand.kt:297-305`).
    ///
    /// An empty scan emits `[]` and no message: the JSON *is* the answer, and a
    /// human-readable "all settled" on stdout would be the C7 leak this mode exists
    /// to avoid.
    async fn run_json_mode(&self, io: &mut dyn Console) -> Result<()> {
        let collected = self.collect_actions(io).await?;
        io.out(&json_body(&collected.actions)?);
        Ok(())
    }

    /// `runDryRunMode` (`SettleCommand.kt:307-323`).
    async fn run_dry_run_mode(&self, io: &mut dyn Console) -> Result<()> {
        let collected = self.collect_actions(io).await?;
        if collected.actions.is_empty() {
            // For an explicit range, empty can mean "no Chrono entries, or none
            // mapped" — already said on stderr — rather than "settled", so the
            // message stays neutral.
            let message = if self.explicit_range() {
                "No actions to settle for this range.".to_string()
            } else {
                nothing_to_settle_message(&collected.not_final, self.today)
            };
            io.out(&message);
            return Ok(());
        }
        io.out(&render_day_summary(&collected.actions));
        Ok(())
    }

    /// `runBatchMode` (`SettleCommand.kt:151-182`). One question, three answers,
    /// no loop — a separate surface from the day-by-day prompt and not a special
    /// case of it.
    async fn run_batch_mode(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        io: &mut dyn Console,
    ) -> Result<()> {
        let actions = self.prepare_actions(from, to, io).await?;
        if actions.is_empty() {
            return Ok(());
        }

        // C7: no console to prompt on, so the readable summary stands in. Without
        // this the run falls through `readLine()` → `None` → `Cancelled.` and looks
        // like a decision somebody made.
        if !io.present() {
            io.out(&render_day_summary(&actions));
            return Ok(());
        }

        io.blank();
        io.out(&draft_table(&actions));
        let warning = under_eight_warning(&actions);
        if let Some(text) = &warning {
            io.out(text);
        }
        io.out(batch_prompt(warning.is_some()));

        match batch_choice(read_choice(io).as_deref()) {
            BatchChoice::Approve => apply_all(&actions, self.tt_client, io).await?,
            BatchChoice::Cancel => io.out("Cancelled."),
            BatchChoice::Unknown => io.out("Unknown option. Cancelled."),
        }
        Ok(())
    }

    /// `runDayByDayMode` (`SettleCommand.kt:325-409`) — the interactive loop.
    ///
    /// D4: the `[A]` branch writes to the live portal and this run never executes
    /// it. Everything it depends on is reachable from the pure layer above.
    async fn run_day_by_day_mode(&self, io: &mut dyn Console) -> Result<()> {
        let scan = self.find_unfilled_days(io).await?;
        if scan.unfilled_days.is_empty() {
            io.out(&nothing_to_settle_message(&scan.not_final, self.today));
            return Ok(());
        }

        // C7 again, and the reason this branch fetches every day up front: with no
        // console there is nothing to answer the per-day prompt.
        if !io.present() {
            let mut all = Vec::new();
            for day in &scan.unfilled_days {
                all.extend(self.prepare_actions(*day, *day, io).await?);
            }
            io.out(&render_day_summary(&all));
            return Ok(());
        }

        io.out(&format!("{} days to settle:", scan.unfilled_days.len()));
        for day in &scan.unfilled_days {
            let hours = scan.devpro_hours_by_day.get(day).copied().unwrap_or(0.0);
            // `:346` — `< 0.01`, not `== 0.0`: a day holding six minutes reads as
            // empty in this listing.
            let hours_info = if hours < 0.01 {
                String::new()
            } else {
                format!(" ({}h)", java_fmt(hours, 1))
            };
            io.out(&format!(
                "  {day} {}{hours_info}",
                weekday_abbreviation(*day)
            ));
        }
        io.blank();

        for day in &scan.unfilled_days {
            let devpro_hours = scan.devpro_hours_by_day.get(day).copied().unwrap_or(0.0);
            let current_hours = if devpro_hours < 0.01 {
                "empty".to_string()
            } else {
                format!("{}h logged", java_fmt(devpro_hours, 1))
            };
            io.out(&format!(
                "\u{2550}\u{2550}\u{2550} {day} {} ({current_hours}) \u{2550}\u{2550}\u{2550}",
                weekday_name(*day)
            ));

            let actions = self.prepare_actions(*day, *day, io).await?;
            if actions.is_empty() {
                // The trailing `\n` is the incumbent's own: `echo` adds one more, so
                // this is a line and then a blank line.
                io.out("No entries for this day.\n");
                continue;
            }

            let mut current = actions;
            loop {
                io.out(&draft_table(&current));
                let warning = under_eight_warning(&current);
                if let Some(text) = &warning {
                    io.out(text);
                }
                io.out(day_prompt(warning.is_some()));

                match day_choice(read_choice(io).as_deref()) {
                    DayChoice::Approve => {
                        apply_all(&current, self.tt_client, io).await?;
                        io.blank();
                        break;
                    }
                    DayChoice::Edit => current = edit_entry(&current, io),
                    DayChoice::Delete => current = delete_entry(&current, io),
                    DayChoice::Skip => {
                        io.out("Skipped.\n");
                        break;
                    }
                    // Both of these abandon every remaining day, not just this one.
                    DayChoice::Cancel => {
                        io.out("Cancelled.");
                        return Ok(());
                    }
                    DayChoice::Unknown => {
                        io.out("Unknown option. Cancelled.");
                        return Ok(());
                    }
                }
            }
        }

        io.out("Done! All unfilled days processed.");
        Ok(())
    }
}

/// Appends `name` under `date`, keeping both levels in first-encounter order and
/// both free of duplicates — Kotlin's `.distinct()` on a list built in that order.
fn push_name(slots: &mut Vec<(NaiveDate, Vec<String>)>, date: NaiveDate, name: &str) {
    match slots.iter_mut().find(|(known, _)| *known == date) {
        Some((_, names)) => {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
        None => slots.push((date, vec![name.to_string()])),
    }
}

/// C29 — `Json { prettyPrint = true }.encodeToString(ListSerializer(...))`.
///
/// kotlinx-serialization's pretty printer indents with **four** spaces and
/// `serde_json`'s `PrettyFormatter` defaults to two, so the formatter is
/// constructed rather than taken from `to_string_pretty`. Verified against
/// `~/.cache/tt-devpro-rewrite/baseline/settle-json.out`, whose second line opens
/// with four spaces and whose third with eight.
///
/// The field-level part of C29 is on the model: kotlinx omits a property that
/// equals its declared default, which `SettleAction`'s `skip_serializing_if`
/// attributes reproduce.
pub fn json_body(actions: &[SettleAction]) -> Result<String> {
    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut buffer,
        serde_json::ser::PrettyFormatter::with_indent(b"    "),
    );
    actions
        .serialize(&mut serializer)
        .context("encoding the proposed actions as JSON")?;
    String::from_utf8(buffer).context("the JSON encoder produced invalid UTF-8")
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// `SettleCommand.run`'s two catch clauses (`SettleCommand.kt:104-109`).
///
/// `ApiException` gets its own prefix; everything else is `✗ Error: `. Both go to
/// stderr in every mode.
///
/// The `{error:#}` is anyhow's whole chain joined with `: ` rather than the
/// outermost message alone. For the one error path that is captured — the D2
/// crash, `~/.cache/tt-devpro-rewrite/baseline/settle-range-aug-json.err`, which
/// carries the project-resolution message and its list of available projects — the
/// two renderings are the same string, because that error is a `bail!` with no
/// context on top of it. They differ where the port adds context the incumbent has
/// no counterpart for, a failed HTTP request being the case: the outermost message
/// alone would be `requesting http://localhost:9247/api/time-entries` with the
/// refusal that caused it dropped.
fn report_failure(error: &anyhow::Error, io: &mut dyn Console) {
    match error.downcast_ref::<ApiError>() {
        Some(api) => io.err(&format!("\u{2717} API Error: {api}")),
        None => io.err(&format!("\u{2717} Error: {error:#}")),
    }
}

/// The body of `SettleCommand.run` after the config has loaded
/// (`SettleCommand.kt:88-102`), as one fallible unit so that the two catch clauses
/// have a single `Err` to look at.
///
/// `session_cookie()` is inside it, which the incumbent's `getSessionCookie()` at
/// `:86` is not — it sits outside the `try`, so a missing `~/.tt-cookie` throws a
/// raw stack trace out of Clikt. D3 names that as a fix: the same message on
/// stderr and a non-zero code, which is what the *expired*-cookie path already did.
async fn dispatch(
    args: &SettleArgs,
    config: &Config,
    today: NaiveDate,
    io: &mut dyn Console,
) -> Result<()> {
    let chrono_client = ChronoClient::new(&config.chrono_api)?;
    let tt_client = TtApiClient::new(crate::cookie::session_cookie()?)?;
    // Walks the knowledge base for `Calendar` directories, so it is built once per
    // run and not once per day of a 45-day scan.
    let normalizer = TimeNormalizer::new();

    let settle = Settle {
        args,
        config,
        chrono_client: &chrono_client,
        tt_client: &tt_client,
        normalizer: &normalizer,
        today,
    };

    // `:90-102`, in order: `--json` wins over `--dry-run`, an explicit range wins
    // over the scan, and the scan is the default.
    if args.json {
        settle.run_json_mode(io).await
    } else if args.dry_run {
        settle.run_dry_run_mode(io).await
    } else if settle.explicit_range() {
        let range = settle.resolve_range(io);
        settle.run_batch_mode(range.from, range.to, io).await
    } else {
        settle.run_day_by_day_mode(io).await
    }
}

/// `tt-devpro settle`.
///
/// D3: the message goes to stderr exactly where the incumbent puts it, and the
/// process exits non-zero instead of zero. Returning [`Outcome`] rather than a
/// `Result` is what keeps `main` from rendering the same failure a second time in
/// a shape nothing measured.
pub async fn run(args: &SettleArgs, io: &mut dyn Console) -> Outcome {
    // `:81-87` — the config's own catch, whose message has no `Error: ` in it.
    let config = match crate::config::load() {
        Ok(config) => config,
        Err(error) => {
            io.err(&format!("\u{2717} {error:#}"));
            return Outcome::Failed;
        }
    };

    // `LocalDate.now()`, read once. See [`Settle`].
    let today = Local::now().date_naive();

    match dispatch(args, &config, today, io).await {
        Ok(()) => Outcome::Ok,
        Err(error) => {
            report_failure(&error, io);
            Outcome::Failed
        }
    }
}
