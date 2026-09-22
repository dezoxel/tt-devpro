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
//!    are contracts and names all five sites, and all five of them are here
//!    (`SettleCommand.kt:604,618,771,844,850`). [`java_to_int`] is the truncating
//!    one, and
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
//! [`Settle::prepare_actions`]: project ids are resolved per day rather than once for the
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
    action_label, clean_chrono_entry, entry_type, render_day_summary, task_title, under_eight_days,
    weekday_abbreviation, weekday_name,
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

/// `SettleCommand.kt:55-68`.
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
/// idea, six lines apart in the incumbent; collapsing them into one is the tidy-up
/// C27 exists to prevent, so both are here.
fn truncate_to_quarter_max_quarter(x: f64) -> f64 {
    java_max(
        QUARTER_HOUR,
        f64::from(java_to_int(x / QUARTER_HOUR)) * QUARTER_HOUR,
    )
}

// ---------------------------------------------------------------------------
// C22 — the explicit range
// ---------------------------------------------------------------------------

/// `SettleCommand.kt:115`'s `ResolvedRange`.
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
/// (`SettleCommand.kt:99`, `:286`) can stay the only place a stream is chosen.
pub fn resolve_range(
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    today: NaiveDate,
    cutoff: NaiveDate,
) -> (ResolvedRange, Option<String>) {
    let range = ResolvedRange {
        // `LocalDate.now().withDayOfMonth(1)`. Day 1 exists in every month, so the
        // `expect` is unreachable for any date `chrono` can represent.
        from: from.unwrap_or_else(|| today.with_day(1).expect("every month has a first day")),
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
        match groups
            .iter_mut()
            .find(|(date, _)| *date == action.aggregate.date)
        {
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
            Some(current)
                if actions[current].normalized_hours < actions[index].normalized_hours =>
            {
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
pub fn adjust_to_eight_hours(
    actions: &[SettleAction],
    max_synthetic_hours: f64,
) -> Vec<SettleAction> {
    let mut out: Vec<SettleAction> = Vec::with_capacity(actions.len());
    for (_, day) in group_by_date_in_encounter_order(actions) {
        out.extend(adjust_day(actions, &day, max_synthetic_hours));
    }
    out
}

fn adjust_day(
    actions: &[SettleAction],
    day: &[usize],
    max_synthetic_hours: f64,
) -> Vec<SettleAction> {
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

/// `SettleCommand.kt:483-575` — everything after the project ids are resolved.
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
        left.aggregate
            .date
            .cmp(&right.aggregate.date)
            .then_with(|| {
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

/// `showDraftTable` (`SettleCommand.kt:635-711`), returned as one block instead of
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

/// `if (s.length > w) s.take(w - 1) + "…" else s` — `SettleCommand.kt:680-682`.
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
    io.read_line().map(|line| kotlin_trim(&line).to_lowercase())
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

/// `applyAll` (`SettleCommand.kt:858-909`). **The only function here that writes to
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
                Ok(request) => client
                    .update_worklog(&request)
                    .await
                    .map(|_| Some("Updated")),
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

/// `SettleCommand.kt:197` — `today.minusDays(45)`. Named here because the number
/// is the whole of the scan's lower bound and the project's own CLAUDE.md quotes
/// it ("Scans the last 45 days").
const SCAN_DAYS: u64 = 45;

/// Every month the closed interval `[start, end]` touches, as the first day of
/// each, in chronological order.
///
/// `SettleCommand.kt:209-214` builds this with a `mutableSetOf` and a `while
/// (!current.isAfter(cutoff))`, so the bound is inclusive and an interval whose
/// ends sit in one month yields exactly one month.
///
/// **An inverted interval is not automatically empty**, which a test caught this
/// comment claiming. The loop starts at `start.withDayOfMonth(1)`, so `[09-20,
/// 09-10]` still yields September — the walk is empty only once that first-of-month
/// date is itself past `end`, i.e. when the inversion crosses a month boundary.
/// That is why [`PeriodBudgets::calculate_if_configured`] carries its own
/// empty-set fallback rather than relying on this one never returning nothing.
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

/// `LocalDate.parse(it.date.substring(0, 10))` — `SettleCommand.kt:223,445`.
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
/// then `sorted()` in Kotlin (`SettleCommand.kt:238-243`); the sort is total over
/// dates, so the first-encounter order `distinct` preserves is not observable and
/// a `Vec` with a membership test reproduces it either way.
fn chrono_days(entries: &[crate::model::ChronoTimeEntry]) -> Result<Vec<NaiveDate>> {
    chrono_days_in_zone(entries, &Local)
}

/// The body of [`chrono_days`] with the zone as a parameter.
///
/// The same move as [`aggregator::aggregate_in_zone`], for the same reason: the
/// incumbent reads `ZoneId.systemDefault()` inline, production goes on passing
/// `Local` through [`chrono_days`], and a C2 test gets to assert a fixed offset
/// instead of asserting whatever zone the machine running it happens to sit in.
fn chrono_days_in_zone<Tz: chrono::TimeZone>(
    entries: &[crate::model::ChronoTimeEntry],
    zone: &Tz,
) -> Result<Vec<NaiveDate>> {
    let mut days: Vec<NaiveDate> = Vec::new();
    for entry in entries {
        let day = aggregator::entry_local_date(&entry.start_time, zone)?;
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

/// `collectActions`' return (`SettleCommand.kt:268-271`).
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

    /// [`explicit_range`] against this run's arguments.
    fn explicit_range(&self) -> bool {
        explicit_range(self.args)
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
    async fn existing_worklogs(
        &self,
        months: &[NaiveDate],
    ) -> Result<Vec<(WorklogDetail, NaiveDate)>> {
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

    /// `prepareActions` (`SettleCommand.kt:411-575`) — the network half; the
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
    /// `SettleCommand.kt:465-474` asks the portal for the assignments held on
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
            let resolution = aggregator::resolve_project_ids(
                names,
                &response.projects,
                &self.config.project_ids,
            )?;
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

    /// `runBatchMode` (`SettleCommand.kt:151-181`). One question, three answers,
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

    /// The incumbent's `if` chain (`SettleCommand.kt:91-104`) reduced to its wiring
    /// alone: which run serves the mode [`mode`] has already named.
    ///
    /// It sits on `Settle` because every arm's target is already a `&self` method
    /// and both callers hold a `Settle` by the time they route — `dispatch` one
    /// built from the real clients, the test harness one built from stubs. The four
    /// arms used to exist twice, verbatim, and only the harness's copy was ever
    /// executed by a test, so the production copy could have exchanged two arms and
    /// stayed green. Keeping them in one place is the whole point of the method; do
    /// not inline it back into either caller.
    async fn run_chosen_mode(&self, io: &mut dyn Console) -> Result<()> {
        match mode(self.args) {
            Mode::Json => self.run_json_mode(io).await,
            Mode::DryRun => self.run_dry_run_mode(io).await,
            Mode::Batch => {
                let range = self.resolve_range(io);
                self.run_batch_mode(range.from, range.to, io).await
            }
            Mode::DayByDay => self.run_day_by_day_mode(io).await,
        }
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

/// `SettleCommand.run`'s two catch clauses (`SettleCommand.kt:105-108`).
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

/// Which of the four modes an invocation selects — C34.
///
/// The incumbent has no such type: its four arms are the four branches of one `if`
/// chain inside `SettleCommand.run` (`SettleCommand.kt:91-104`). Naming the choice
/// is what lets the precedence be asserted without a portal, a cookie or a clock —
/// see [`mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Json,
    DryRun,
    Batch,
    DayByDay,
}

/// C34, the incumbent's mode precedence (`SettleCommand.kt:91-104`), as a function
/// of the arguments alone: `--json` wins over `--dry-run`, an explicit range wins
/// over the scan, and the scan is the default.
///
/// Pure on purpose. `dispatch` builds both clients from [`crate::api::portal::BASE_URL`]
/// and `~/.tt-cookie` inline, so the chain that used to live there could not be
/// driven from a test, and the test harness carried a second verbatim copy of it
/// that no test compared against the first. The arms now exist once, in
/// [`Settle::run_chosen_mode`], which both callers go through; this function is the
/// precedence alone and is asserted on all four outcomes directly. `mode` names the
/// mode and resolves nothing — the range is still resolved by its caller, inside
/// the `Batch` arm.
fn mode(args: &SettleArgs) -> Mode {
    if args.json {
        Mode::Json
    } else if args.dry_run {
        Mode::DryRun
    } else if explicit_range(args) {
        Mode::Batch
    } else {
        Mode::DayByDay
    }
}

/// True when the invocation named at least one end of a range, which is the
/// condition three separate places in the incumbent branch on (`SettleCommand.kt:97`,
/// `:283`, `:317`).
///
/// It is an `or`: one end is enough, because `resolveRange` defaults the other
/// (C22). C34's third branch is this predicate; free rather than a method so that
/// [`mode`] needs no [`Settle`].
fn explicit_range(args: &SettleArgs) -> bool {
    args.from.is_some() || args.to.is_some()
}

/// The body of `SettleCommand.run` after the config has loaded
/// (`SettleCommand.kt:87-104`), as one fallible unit so that the two catch clauses
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

    // `:91-104`. The precedence itself is C34 and lives in `mode`; the wiring from
    // a named mode to the run that serves it is `run_chosen_mode`, and this caller
    // and the test harness share that one copy of it. So the wiring does have a
    // unit-test seam: the harness builds the same `Settle` from stubs and routes
    // through the same method, and an exchanged pair of arms fails there. What is
    // still unique to this function is the composition above it — the two clients
    // built from `BASE_URL` and `~/.tt-cookie` inline, which no unit test can
    // construct — and that live composition is what the differential parity
    // harness covers, where the `settle-dryrun-and-json` case passes both flags and
    // expects JSON.
    settle.run_chosen_mode(io).await
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use chrono::FixedOffset;

    use super::*;
    use crate::model::{ChronoProject, ChronoTimeEntry, CurrentUser};

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    fn d(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("a test date")
    }

    /// UTC-4, the zone the live Chrono data was recorded in. Fixed so that a C2
    /// assertion is about the rule and not about the machine running it.
    fn edt() -> FixedOffset {
        FixedOffset::west_opt(4 * 3600).expect("UTC-4 is a valid offset")
    }

    /// A [`Console`] that keeps what was written and hands out canned input.
    ///
    /// `present` defaults to **true**, the interactive case, because the
    /// non-interactive one is the exception each test that needs it names.
    struct FakeConsole {
        out: Vec<String>,
        err: Vec<String>,
        input: VecDeque<String>,
        present: bool,
    }

    impl FakeConsole {
        fn new() -> Self {
            FakeConsole {
                out: Vec::new(),
                err: Vec::new(),
                input: VecDeque::new(),
                present: true,
            }
        }

        /// Canned answers, consumed in order. Running out is EOF, which is what a
        /// closed stdin is, so a test that under-feeds the loop exercises the EOF
        /// contract rather than hanging.
        fn typing(lines: &[&str]) -> Self {
            let mut console = FakeConsole::new();
            console.input = lines.iter().map(|line| (*line).to_string()).collect();
            console
        }

        fn absent(mut self) -> Self {
            self.present = false;
            self
        }

        fn out_text(&self) -> String {
            self.out.join("\n")
        }

        fn err_text(&self) -> String {
            self.err.join("\n")
        }
    }

    impl Console for FakeConsole {
        fn out(&mut self, line: &str) {
            self.out.push(line.to_string());
        }

        fn err(&mut self, line: &str) {
            self.err.push(line.to_string());
        }

        fn read_line(&mut self) -> Option<String> {
            self.input.pop_front()
        }

        fn present(&self) -> bool {
            self.present
        }
    }

    /// A [`SettleAction`] builder, since the struct has thirteen fields and a test
    /// varies two of them.
    ///
    /// `normalized_hours` follows `total_hours` unless a test separates them, which
    /// is what the `orig→norm` column in [`draft_table`] keys off.
    struct Row {
        date: NaiveDate,
        devpro_project: String,
        chrono_project: String,
        title: String,
        total_hours: f64,
        normalized_hours: f64,
        is_meeting: bool,
        is_filler: bool,
        is_borrowed: bool,
        is_manually_fixed: bool,
        source_date: Option<NaiveDate>,
        project_id: String,
        action: ActionType,
        billability: String,
        existing_worklog_id: Option<String>,
        descriptions: Option<Vec<String>>,
    }

    impl Row {
        fn new(date: &str, devpro_project: &str, hours: f64) -> Self {
            Row {
                date: d(date),
                devpro_project: devpro_project.to_string(),
                chrono_project: "Work - DevPro - Work".to_string(),
                title: "Development".to_string(),
                total_hours: hours,
                normalized_hours: hours,
                is_meeting: false,
                is_filler: false,
                is_borrowed: false,
                is_manually_fixed: false,
                source_date: None,
                project_id: "id-1".to_string(),
                action: ActionType::Create,
                billability: "Billable".to_string(),
                existing_worklog_id: None,
                descriptions: None,
            }
        }

        fn title(mut self, title: &str) -> Self {
            self.title = title.to_string();
            self
        }

        fn chrono_project(mut self, project: &str) -> Self {
            self.chrono_project = project.to_string();
            self
        }

        fn original(mut self, hours: f64) -> Self {
            self.total_hours = hours;
            self
        }

        fn meeting(mut self) -> Self {
            self.is_meeting = true;
            self
        }

        fn filler(mut self) -> Self {
            self.is_filler = true;
            self.chrono_project = "[filler]".to_string();
            self
        }

        fn borrowed(mut self, source: &str) -> Self {
            self.is_borrowed = true;
            self.chrono_project = "[borrowed]".to_string();
            self.source_date = Some(d(source));
            self
        }

        fn fixed(mut self) -> Self {
            self.is_manually_fixed = true;
            self
        }

        fn project_id(mut self, id: &str) -> Self {
            self.project_id = id.to_string();
            self
        }

        fn action(mut self, action: ActionType) -> Self {
            self.action = action;
            self
        }

        /// The aggregate's billability, which every other fixture leaves at
        /// `Billable`. Needed because it travels verbatim into both write bodies
        /// and into the `--json` output.
        fn billability(mut self, billability: &str) -> Self {
            self.billability = billability.to_string();
            self
        }

        /// The id an UPDATE carries. `build` leaves it `None`, which is the state
        /// [`update_request`] refuses.
        fn existing(mut self, id: &str) -> Self {
            self.existing_worklog_id = Some(id.to_string());
            self
        }

        /// Separates `normalized_hours` from the aggregate's `total_hours`, which is
        /// what the `orig→norm` column keys off and what makes two actions sharing
        /// one aggregate distinguishable.
        fn normalized(mut self, hours: f64) -> Self {
            self.normalized_hours = hours;
            self
        }

        fn descriptions(mut self, descriptions: &[&str]) -> Self {
            self.descriptions = Some(descriptions.iter().map(|t| (*t).to_string()).collect());
            self
        }

        fn build(self) -> SettleAction {
            let descriptions = self
                .descriptions
                .unwrap_or_else(|| vec![self.title.clone()]);
            SettleAction {
                aggregate: DayProjectAggregate {
                    date: self.date,
                    chrono_project: self.chrono_project,
                    total_hours: self.total_hours,
                    descriptions,
                    devpro_project_name: self.devpro_project,
                    billability: self.billability,
                    max_hours: None,
                },
                normalized_hours: self.normalized_hours,
                is_meeting: self.is_meeting,
                is_filler: self.is_filler,
                is_borrowed: self.is_borrowed,
                source_date: self.source_date,
                task_title: self.title,
                devpro_project_id: self.project_id,
                action: self.action,
                existing_worklog_id: self.existing_worklog_id,
                is_manually_fixed: self.is_manually_fixed,
            }
        }
    }

    fn hours(actions: &[SettleAction]) -> Vec<f64> {
        actions.iter().map(|a| a.normalized_hours).collect()
    }

    fn worklog(unique_id: &str, project_id: &str, title: &str) -> WorklogDetail {
        WorklogDetail {
            unique_id: unique_id.to_string(),
            project_unique_id: project_id.to_string(),
            project_short_name: "Some Project".to_string(),
            task_title: title.to_string(),
            billability: "Billable".to_string(),
            logged_hours: 4.0,
            is_deletable: true,
            expense_type: None,
        }
    }

    /// A portal client pointed at a stub origin, with the C31 bounds supplied so
    /// that a test that means to hit a timeout does not wait fifteen seconds for it.
    /// The cookie is the same placeholder `portal.rs`'s own tests use.
    fn stub_client(base_url: &str) -> TtApiClient {
        TtApiClient::with_base_url(
            "session=test",
            base_url,
            crate::api::REQUEST_TIMEOUT,
            crate::api::CONNECT_TIMEOUT,
        )
        .expect("a client against the stub")
    }

    fn chrono_entry(id: i64, start: &str) -> ChronoTimeEntry {
        ChronoTimeEntry {
            id,
            description: Some("Work".to_string()),
            start_time: start.to_string(),
            end_time: None,
            duration: Some(3600),
            project: Some(ChronoProject {
                id: 1,
                name: "Work - DevPro - Work".to_string(),
                color: "#000".to_string(),
                aspect: None,
            }),
            aspect: None,
        }
    }

    // -----------------------------------------------------------------------
    // C34 — mode precedence (`SettleCommand.kt:91-104`)
    // -----------------------------------------------------------------------
    //
    // `--json` beats `--dry-run`, an explicit range beats the scan, the scan is
    // the default. Do not read this off C7 or C22: C7 owns the `quiet` predicate,
    // which merely happens to be built from the same pair of flags, and C22 owns
    // how a range is resolved, not which mode was chosen.
    //
    // The differential parity harness cannot establish C34: every argv it is
    // allowed to run carries `--dry-run` or `--json`, so `Batch` and `DayByDay`
    // are never taken there and only one of the three precedence edges — `--json`
    // over `--dry-run`, its `settle-dryrun-and-json` case — is exercised.
    // Recorded as limitation 3 in `~/.cache/tt-devpro-rewrite/parity/README.md`,
    // "What this instrument CANNOT establish". These tests are the other two
    // edges and the default, and they are why `mode` is a function of
    // `SettleArgs` alone.

    /// C34's first branch. `SettleCommand.kt:91-93`: `--json` on its own routes
    /// to `runJsonMode`.
    #[test]
    fn json_alone_selects_json_mode() {
        let args = SettleArgs {
            json: true,
            ..SettleArgs::default()
        };
        assert_eq!(mode(&args), Mode::Json);
    }

    /// C34's second branch. `SettleCommand.kt:94-96`: `--dry-run` on its own
    /// routes to `runDryRunMode`, not to the interactive scan.
    #[test]
    fn dry_run_alone_selects_dry_run_mode() {
        let args = SettleArgs {
            dry_run: true,
            ..SettleArgs::default()
        };
        assert_eq!(mode(&args), Mode::DryRun);
    }

    /// C34's first precedence edge. `SettleCommand.kt:91-96`: the `--json` test
    /// comes first, so both flags together are JSON. The incumbent's own
    /// `--dry-run` help text says it — "(--json wins if both are given)". Swap the
    /// two branches and this fails.
    #[test]
    fn json_wins_over_dry_run_when_both_are_given() {
        let args = SettleArgs {
            json: true,
            dry_run: true,
            ..SettleArgs::default()
        };
        assert_eq!(mode(&args), Mode::Json);
    }

    /// C34's first edge again, with C22's range present. `SettleCommand.kt:91-99`:
    /// the `--json` test is
    /// unconditional, not "JSON unless a range was named". Without this case a
    /// port could guard the first branch with `json && !explicit_range(args)` and
    /// still pass `json_wins_over_dry_run_when_both_are_given`, because that one
    /// names no range.
    #[test]
    fn json_wins_over_dry_run_even_with_an_explicit_range() {
        let args = SettleArgs {
            from: Some(d("2026-09-01")),
            to: Some(d("2026-09-15")),
            json: true,
            dry_run: true,
            ..SettleArgs::default()
        };
        assert_eq!(mode(&args), Mode::Json);
    }

    /// C34's second precedence edge. `SettleCommand.kt:94-99`: the `--dry-run`
    /// test is reached before the range test, so a dry run over a named range is
    /// still a
    /// dry run and still writes nothing. Hoist the range branch above it and this
    /// fails — with `Batch`, which applies worklogs.
    #[test]
    fn dry_run_wins_over_an_explicit_range() {
        let args = SettleArgs {
            from: Some(d("2026-09-01")),
            to: Some(d("2026-09-15")),
            dry_run: true,
            ..SettleArgs::default()
        };
        assert_eq!(mode(&args), Mode::DryRun);
    }

    /// C34's third precedence edge, held as a contrast so the swap is visible in
    /// one place. `SettleCommand.kt:97-104`: the same arguments minus
    /// the range ends fall through to `runDayByDayMode`. Exchange the last two
    /// branches and both halves fail at once.
    #[test]
    fn an_explicit_range_displaces_the_scan_that_would_otherwise_run() {
        let scan = SettleArgs::default();
        let ranged = SettleArgs {
            from: Some(d("2026-09-01")),
            to: Some(d("2026-09-15")),
            ..SettleArgs::default()
        };
        assert_eq!(mode(&scan), Mode::DayByDay);
        assert_eq!(mode(&ranged), Mode::Batch);
    }

    /// C34's third branch, whose condition is C22's. `SettleCommand.kt:97` is
    /// `from != null || to != null`, so one end is
    /// enough — `resolveRange` defaults `--to` to the cutoff. Turn the `||` into
    /// an `&&` and this is the first of the three tests that catches it.
    #[test]
    fn a_from_alone_is_enough_to_select_batch_mode() {
        let args = SettleArgs {
            from: Some(d("2026-09-01")),
            ..SettleArgs::default()
        };
        assert_eq!(mode(&args), Mode::Batch);
    }

    /// C34's third branch, the other end. `SettleCommand.kt:97`, same `||`: a
    /// lone `--to` is a
    /// range too, with `--from` defaulting to the 1st of the month. An `&&` fails
    /// here as well, and a port that only checked `from` fails here and nowhere
    /// else.
    #[test]
    fn a_to_alone_is_enough_to_select_batch_mode() {
        let args = SettleArgs {
            to: Some(d("2026-09-15")),
            ..SettleArgs::default()
        };
        assert_eq!(mode(&args), Mode::Batch);
    }

    /// C34's third branch, both ends. `SettleCommand.kt:97`: the ordinary
    /// spelling, and the one
    /// case an exclusive-or would let through while still passing the two
    /// single-end tests above.
    #[test]
    fn both_range_ends_together_select_batch_mode() {
        let args = SettleArgs {
            from: Some(d("2026-09-01")),
            to: Some(d("2026-09-15")),
            ..SettleArgs::default()
        };
        assert_eq!(mode(&args), Mode::Batch);
    }

    /// C34's default branch, which is the one interactive mode — and therefore
    /// the only one C7 calls non-quiet. `SettleCommand.kt:101-104` — no flag and
    /// no range end
    /// falls all the way through to `runDayByDayMode`, which prompts. Any branch
    /// rewritten to catch the bare invocation fails here.
    #[test]
    fn neither_a_flag_nor_a_range_end_selects_day_by_day_mode() {
        let args = SettleArgs::default();
        assert_eq!(mode(&args), Mode::DayByDay);
    }

    /// C1, not C34. `--include-today` moves the cutoff (`SettleCommand.kt:77`)
    /// and is absent from the routing switch at `:91-104`, so it must not reach
    /// the mode choice. A port that wrote `explicit_range(args) ||
    /// args.include_today` — plausible, since both concern which days are in
    /// scope — would turn a bare `settle --include-today` into a batch run over a
    /// defaulted range instead of the interactive scan, and fails here.
    #[test]
    fn include_today_is_not_part_of_the_mode_choice() {
        let bare = SettleArgs {
            include_today: true,
            ..SettleArgs::default()
        };
        let ranged = SettleArgs {
            from: Some(d("2026-09-01")),
            include_today: true,
            ..SettleArgs::default()
        };
        assert_eq!(mode(&bare), Mode::DayByDay);
        assert_eq!(mode(&ranged), Mode::Batch);
    }

    // -----------------------------------------------------------------------
    // Kotlin's string-to-number conversions
    // -----------------------------------------------------------------------

    /// `Character.isWhitespace` counts U+001C..U+001F and Rust's `White_Space`
    /// does not, so a `str::trim` port leaves them on and the answer stops being
    /// recognised.
    #[test]
    fn kotlin_trim_strips_the_four_information_separators() {
        assert_eq!(kotlin_trim("\u{001C}\u{001D}a\u{001E}\u{001F}"), "a");
        assert!(
            !"\u{001C}a".trim().is_empty() && "\u{001C}a".trim() == "\u{001C}a",
            "the premise: Rust's own trim leaves U+001C on"
        );
    }

    /// The other direction, and the one with teeth: Rust counts U+00A0, U+2007 and
    /// U+202F as whitespace and Java does not.
    #[test]
    fn kotlin_trim_keeps_the_three_no_break_spaces() {
        assert_eq!(kotlin_trim("\u{00A0}a"), "\u{00A0}a");
        assert_eq!(kotlin_trim("\u{2007}a"), "\u{2007}a");
        assert_eq!(kotlin_trim("\u{202F}a"), "\u{202F}a");
    }

    /// The consequence, spelled out: a line pasted with a no-break space before the
    /// `a` cancels every remaining day under the incumbent. A `str::trim` port
    /// approves and writes the worklogs instead.
    #[test]
    fn a_no_break_space_before_the_approval_cancels_instead_of_approving() {
        let typed = kotlin_trim("\u{00A0}a").to_lowercase();
        assert_eq!(day_choice(Some(&typed)), DayChoice::Unknown);
        assert_eq!(
            day_choice(Some(&"\u{001C}a".trim().to_lowercase())),
            DayChoice::Unknown
        );
        assert_eq!(
            day_choice(Some(&kotlin_trim("\u{001C}a").to_lowercase())),
            DayChoice::Approve
        );
    }

    /// `Character.digit(c, 10)` reads every Unicode decimal digit, not only ASCII.
    #[test]
    fn to_int_or_null_reads_non_ascii_decimal_digits() {
        assert_eq!(to_int_or_null("\u{0663}"), Some(3), "Arabic-Indic three");
        assert_eq!(
            to_int_or_null("\u{FF11}\u{FF12}"),
            Some(12),
            "fullwidth twelve"
        );
        assert_eq!(
            to_int_or_null("\u{1D7CE}"),
            Some(0),
            "mathematical bold zero"
        );
    }

    /// Mixing scripts is allowed — `Character.digit` is per character and nothing
    /// checks that two digits came from one block.
    #[test]
    fn to_int_or_null_accepts_digits_from_two_different_blocks_in_one_number() {
        assert_eq!(to_int_or_null("1\u{0662}"), Some(12));
    }

    /// Kotlin accumulates negatively against the negative limit, so `Int.MIN_VALUE`
    /// parses and its positive counterpart does not.
    #[test]
    fn to_int_or_null_accepts_int_min_and_rejects_everything_past_the_bounds() {
        assert_eq!(to_int_or_null("-2147483648"), Some(i32::MIN));
        assert_eq!(to_int_or_null("2147483647"), Some(i32::MAX));
        assert_eq!(to_int_or_null("2147483648"), None);
        assert_eq!(to_int_or_null("-2147483649"), None);
    }

    /// A number far past any integer width is `null`, not an overflow: the check
    /// fires inside the loop, before the accumulator itself could wrap.
    #[test]
    fn to_int_or_null_returns_none_for_a_twenty_digit_number_rather_than_overflowing() {
        assert_eq!(to_int_or_null("99999999999999999999"), None);
        assert_eq!(to_int_or_null("-99999999999999999999"), None);
    }

    /// The caller has already trimmed, and Kotlin does not trim again — so the
    /// forms `str::parse` would also reject, plus the space it would not.
    #[test]
    fn to_int_or_null_rejects_a_leading_space_a_bare_sign_an_empty_string_and_a_decimal() {
        assert_eq!(to_int_or_null(" 3"), None);
        assert_eq!(to_int_or_null("3 "), None);
        assert_eq!(to_int_or_null("+"), None);
        assert_eq!(to_int_or_null("-"), None);
        assert_eq!(to_int_or_null(""), None);
        assert_eq!(to_int_or_null("3.0"), None);
        assert_eq!(to_int_or_null("3a"), None);
    }

    /// Both signs are accepted, which is the one thing `str::parse` agrees on.
    #[test]
    fn to_int_or_null_takes_an_explicit_plus_sign() {
        assert_eq!(to_int_or_null("+7"), Some(7));
        assert_eq!(to_int_or_null("-7"), Some(-7));
    }

    /// `Double.toInt()` truncates toward zero. `roundToInt()`, which C27 pairs it
    /// with, would give 4 and -4 here.
    #[test]
    fn java_to_int_truncates_toward_zero_rather_than_rounding() {
        assert_eq!(java_to_int(3.9), 3);
        assert_eq!(java_to_int(-3.9), -3);
        assert_eq!(java_to_int(0.9999), 0);
    }

    /// The width is the point: `.toInt()` is a narrowing conversion to 32 bits, so
    /// a port that reads it as `i64` is wrong by 2^32 on a value out of range.
    #[test]
    fn java_to_int_saturates_at_thirty_two_bits_not_sixty_four() {
        assert_eq!(java_to_int(1e18), i32::MAX);
        assert_eq!(java_to_int(-1e18), i32::MIN);
        assert_ne!(i64::from(java_to_int(1e18)), 1_000_000_000_000_000_000_i64);
    }

    #[test]
    fn java_to_int_maps_nan_to_zero() {
        assert_eq!(java_to_int(f64::NAN), 0);
    }

    /// C27's whole point at the two `coerceAtLeast` sites: the value is truncated to
    /// a quarter, never rounded to one. `round_to_quarter(0.74)` is 0.75.
    #[test]
    fn truncate_to_quarter_at_least_quarter_truncates_where_the_normalizer_rounds() {
        assert_eq!(truncate_to_quarter_at_least_quarter(0.74), 0.5);
        assert_eq!(
            crate::service::normalizer::round_to_quarter(0.74),
            0.75,
            "the premise: the normalizer's rounding site gives the other answer"
        );
        assert_eq!(truncate_to_quarter_at_least_quarter(3.99), 3.75);
    }

    #[test]
    fn truncate_to_quarter_at_least_quarter_floors_at_one_quarter() {
        assert_eq!(truncate_to_quarter_at_least_quarter(0.24), QUARTER_HOUR);
        assert_eq!(truncate_to_quarter_at_least_quarter(0.0), QUARTER_HOUR);
        assert_eq!(truncate_to_quarter_at_least_quarter(-5.0), QUARTER_HOUR);
        assert_eq!(truncate_to_quarter_at_least_quarter(0.25), QUARTER_HOUR);
    }

    /// The second spelling, four lines away in the incumbent, with the same two
    /// behaviours. Both are kept because C27 names both sites.
    #[test]
    fn truncate_to_quarter_max_quarter_truncates_and_floors_the_same_way() {
        assert_eq!(truncate_to_quarter_max_quarter(0.74), 0.5);
        assert_eq!(truncate_to_quarter_max_quarter(3.99), 3.75);
        assert_eq!(truncate_to_quarter_max_quarter(0.24), QUARTER_HOUR);
        assert_eq!(truncate_to_quarter_max_quarter(-5.0), QUARTER_HOUR);
    }

    // -----------------------------------------------------------------------
    // C22 — the explicit range
    // -----------------------------------------------------------------------

    /// `LocalDate.now().withDayOfMonth(1)`, and not "45 days back" — the two
    /// coincide for no month.
    #[test]
    fn an_unspecified_from_is_the_first_of_the_month_today_falls_in() {
        let (range, note) = resolve_range(
            None,
            Some(d("2026-09-20")),
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert_eq!(range.from, d("2026-09-01"));
        assert_eq!(note, None);
    }

    /// The upper default is the cutoff, which under `--include-today` is today and
    /// otherwise yesterday. A port defaulting to `today` settles an unfinished day.
    #[test]
    fn an_unspecified_to_is_the_cutoff_and_not_today() {
        let (range, _) = resolve_range(
            Some(d("2026-09-01")),
            None,
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert_eq!(range.to, d("2026-09-21"));

        let (included, _) = resolve_range(
            Some(d("2026-09-01")),
            None,
            d("2026-09-22"),
            d("2026-09-22"),
        );
        assert_eq!(included.to, d("2026-09-22"));
    }

    /// C22's rule: an explicit range is honoured verbatim and the note is
    /// non-blocking. A port that clamped `to` to the cutoff would pass every other
    /// test here.
    #[test]
    fn a_to_past_the_cutoff_is_honoured_and_carries_the_stderr_note() {
        let (range, note) = resolve_range(
            Some(d("2026-09-01")),
            Some(d("2026-09-30")),
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert_eq!(range.to, d("2026-09-30"));
        assert_eq!(
            note.as_deref(),
            Some(
                "\u{2139} Range ends 2026-09-30, past the last completed day (2026-09-21) \u{2014} those days' hours aren't final."
            )
        );
    }

    /// The boundary. `>` and not `>=`, so the cutoff itself is silent.
    #[test]
    fn a_to_exactly_on_the_cutoff_produces_no_note() {
        let (_, note) = resolve_range(
            None,
            Some(d("2026-09-21")),
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert_eq!(note, None);
    }

    /// The comparison is against the cutoff, not today. Under `--include-today` the
    /// two are the same date, and a port comparing with `today` would announce that
    /// the range runs past the last completed day about the very day the flag just
    /// made settleable.
    #[test]
    fn under_include_today_a_to_of_today_produces_no_note() {
        let (range, note) = resolve_range(
            None,
            Some(d("2026-09-22")),
            d("2026-09-22"),
            d("2026-09-22"),
        );
        assert_eq!(range.to, d("2026-09-22"));
        assert_eq!(note, None);

        let (_, without_the_flag) = resolve_range(
            None,
            Some(d("2026-09-22")),
            d("2026-09-22"),
            d("2026-09-21"),
        );
        assert!(
            without_the_flag.is_some(),
            "the premise: without the flag the same date is noted"
        );
    }

    // -----------------------------------------------------------------------
    // C13 — CREATE vs UPDATE
    // -----------------------------------------------------------------------

    /// C13 matches on `(date, projectId)` and **not** on the title, so a day
    /// already holding any worklog for the project becomes an UPDATE of it.
    #[test]
    fn a_worklog_on_the_same_day_and_project_matches_whatever_it_is_called() {
        let existing = vec![(
            worklog("w-1", "p-1", "Something else entirely"),
            d("2026-09-15"),
        )];
        let found = find_existing(d("2026-09-15"), "p-1", &existing);
        assert_eq!(found.map(|w| w.unique_id.as_str()), Some("w-1"));
    }

    #[test]
    fn a_worklog_on_another_day_or_another_project_is_not_a_match() {
        let existing = vec![
            (worklog("w-1", "p-1", "Development"), d("2026-09-15")),
            (worklog("w-2", "p-2", "Development"), d("2026-09-16")),
        ];
        assert!(find_existing(d("2026-09-16"), "p-1", &existing).is_none());
        assert!(find_existing(d("2026-09-15"), "p-2", &existing).is_none());
    }

    /// `firstOrNull`, in the order the portal listed them — a day holding two
    /// worklogs for one project updates the first and leaves the second alone.
    #[test]
    fn the_first_match_wins_when_a_day_holds_two_worklogs_for_one_project() {
        let existing = vec![
            (worklog("w-first", "p-1", "Morning"), d("2026-09-15")),
            (worklog("w-second", "p-1", "Afternoon"), d("2026-09-15")),
        ];
        let found = find_existing(d("2026-09-15"), "p-1", &existing);
        assert_eq!(found.map(|w| w.unique_id.as_str()), Some("w-first"));
    }

    #[test]
    fn an_empty_worklog_list_matches_nothing() {
        assert!(find_existing(d("2026-09-15"), "p-1", &[]).is_none());
    }

    // -----------------------------------------------------------------------
    // The scan window
    // -----------------------------------------------------------------------

    #[test]
    fn a_range_inside_one_month_yields_that_month_once() {
        assert_eq!(
            months_in_range(d("2026-09-03"), d("2026-09-28")),
            vec![d("2026-09-01")]
        );
    }

    /// The `while (!current.isAfter(cutoff))` bound is inclusive, so a range ending
    /// on the first of a month still includes that month — which under D2 is the
    /// difference between fetching its worklogs and not.
    #[test]
    fn the_end_of_the_range_is_inclusive_down_to_the_first_of_its_month() {
        assert_eq!(
            months_in_range(d("2026-01-15"), d("2026-02-01")),
            vec![d("2026-01-01"), d("2026-02-01")]
        );
    }

    /// The 45-day scan crosses a year boundary every December, and `plusMonths` has
    /// to roll the year with it.
    #[test]
    fn a_range_spanning_a_year_boundary_yields_every_month_in_order() {
        assert_eq!(
            months_in_range(d("2025-11-20"), d("2026-02-03")),
            vec![
                d("2025-11-01"),
                d("2025-12-01"),
                d("2026-01-01"),
                d("2026-02-01")
            ]
        );
    }

    /// An inversion inside one month still yields that month, because the loop
    /// starts at the *first* of `start`'s month and 09-01 is not after 09-10. Only
    /// an inversion that crosses a month boundary walks zero times.
    ///
    /// This is the assertion that corrected `months_in_range`'s own doc comment,
    /// which claimed every inverted interval was empty.
    #[test]
    fn an_inversion_inside_one_month_still_yields_that_month() {
        assert_eq!(
            months_in_range(d("2026-09-20"), d("2026-09-10")),
            vec![d("2026-09-01")]
        );
    }

    /// The genuinely empty case: `start`'s first-of-month is past `end`.
    #[test]
    fn an_inversion_across_a_month_boundary_yields_no_months() {
        assert!(months_in_range(d("2026-09-20"), d("2026-08-31")).is_empty());
        assert!(months_in_range(d("2026-09-01"), d("2026-08-31")).is_empty());
    }

    /// Only the date half of the portal's ISO timestamp is read; the time, the
    /// offset and anything after them are ignored.
    #[test]
    fn detail_date_reads_the_date_half_of_the_portal_timestamp_and_ignores_the_rest() {
        assert_eq!(
            detail_date("2026-09-15T00:00:00.000Z").expect("a date"),
            d("2026-09-15")
        );
        assert_eq!(detail_date("2026-09-15").expect("a date"), d("2026-09-15"));
        assert_eq!(
            detail_date("2026-09-15T22:30:00+03:00").expect("a date"),
            d("2026-09-15")
        );
    }

    /// Kotlin's `substring(0, 10)` throws here; this says so instead of indexing
    /// out of range, and the message carries what arrived.
    #[test]
    fn a_portal_date_too_short_to_hold_a_date_is_an_error_naming_what_arrived() {
        let error = detail_date("2026-09").expect_err("too short");
        assert!(
            error.to_string().contains("2026-09"),
            "the message names the value: {error}"
        );
    }

    /// `LocalDate.parse` is strict `ISO_LOCAL_DATE`, so a single-digit month is a
    /// failure rather than a lenient parse — the head here is `2026-9-15T`.
    #[test]
    fn a_portal_date_whose_head_is_not_strict_iso_is_an_error() {
        assert!(detail_date("2026-9-15T00:00:00Z").is_err());
    }

    // -----------------------------------------------------------------------
    // C16 — which days are offered
    // -----------------------------------------------------------------------

    /// The comparison is `< 8.0` on the portal's own figure. A day at exactly 8h is
    /// settled; a hundredth under is not.
    #[test]
    fn a_day_at_exactly_eight_hours_is_settled_and_a_hundredth_under_is_not() {
        let days = vec![d("2026-09-15"), d("2026-09-16")];
        let mut logged = HashMap::new();
        logged.insert(d("2026-09-15"), 8.0);
        logged.insert(d("2026-09-16"), 7.99);
        assert_eq!(unfilled_days(&days, &logged), vec![d("2026-09-16")]);
    }

    /// A day nobody has touched is missing from the portal's map, which is zero
    /// hours and therefore unfilled — the common case, and the one an
    /// `unwrap_or(8.0)` style default would silently drop.
    #[test]
    fn a_day_the_portal_never_mentioned_counts_as_zero_hours_and_is_offered() {
        let days = vec![d("2026-09-15")];
        assert_eq!(unfilled_days(&days, &HashMap::new()), vec![d("2026-09-15")]);
    }

    /// 2026-09-19 is a Saturday and 2026-09-20 a Sunday. Neither is ever offered,
    /// whatever the portal says about them.
    #[test]
    fn weekends_are_never_offered_even_at_zero_hours() {
        let days = vec![
            d("2026-09-18"),
            d("2026-09-19"),
            d("2026-09-20"),
            d("2026-09-21"),
        ];
        assert_eq!(
            unfilled_days(&days, &HashMap::new()),
            vec![d("2026-09-18"), d("2026-09-21")]
        );
    }

    /// 2026-09-07 is Labor Day. The holiday predicate is consulted for the same
    /// reason the weekend test is: neither day is expected to hold eight hours.
    #[test]
    fn a_us_federal_holiday_is_never_offered() {
        let days = vec![d("2026-09-04"), d("2026-09-07"), d("2026-09-08")];
        assert_eq!(
            unfilled_days(&days, &HashMap::new()),
            vec![d("2026-09-04"), d("2026-09-08")]
        );
    }

    /// The input order is kept — the day-by-day listing prints these in the order
    /// this returns them.
    #[test]
    fn the_offered_days_keep_the_order_they_arrived_in() {
        let days = vec![d("2026-09-18"), d("2026-09-15"), d("2026-09-16")];
        assert_eq!(unfilled_days(&days, &HashMap::new()), days);
    }

    // -----------------------------------------------------------------------
    // C2 — an entry belongs to its local day
    // -----------------------------------------------------------------------

    /// 04:30Z on the 16th is 00:30 on the 16th in UTC-4, and 23:30 on the 15th one
    /// hour earlier. The padding that fetches `cutoff + 1 day` on the UTC axis is
    /// safe only because of this re-dating.
    #[test]
    fn an_entry_is_dated_by_its_local_day_and_not_by_its_utc_day() {
        let entries = vec![
            chrono_entry(1, "2026-09-16T03:30:00Z"),
            chrono_entry(2, "2026-09-16T04:30:00Z"),
        ];
        assert_eq!(
            chrono_days_in_zone(&entries, &edt()).expect("dates"),
            vec![d("2026-09-15"), d("2026-09-16")]
        );
    }

    /// `distinct()` — several entries on one local day yield it once.
    #[test]
    fn several_entries_on_one_local_day_yield_that_day_once() {
        let entries = vec![
            chrono_entry(1, "2026-09-16T13:00:00Z"),
            chrono_entry(2, "2026-09-16T15:00:00Z"),
            chrono_entry(3, "2026-09-16T17:00:00Z"),
        ];
        assert_eq!(
            chrono_days_in_zone(&entries, &edt()).expect("dates"),
            vec![d("2026-09-16")]
        );
    }

    /// `sorted()` after `distinct()`, so the caller's finality split works on an
    /// ordered list however the entries arrived.
    #[test]
    fn the_local_days_come_back_sorted_whatever_order_the_entries_arrived_in() {
        let entries = vec![
            chrono_entry(1, "2026-09-18T13:00:00Z"),
            chrono_entry(2, "2026-09-14T13:00:00Z"),
            chrono_entry(3, "2026-09-16T13:00:00Z"),
        ];
        assert_eq!(
            chrono_days_in_zone(&entries, &edt()).expect("dates"),
            vec![d("2026-09-14"), d("2026-09-16"), d("2026-09-18")]
        );
    }

    /// A numeric offset, which `Instant.parse` accepts on JDK 12+ and 956 of the
    /// live window's entries carry. 01:00+03:00 is 22:00Z the day before, which in
    /// UTC-4 is the 17th.
    #[test]
    fn a_start_time_with_a_numeric_offset_is_read_and_re_dated() {
        let entries = vec![chrono_entry(1, "2026-09-18T01:00:00+03:00")];
        assert_eq!(
            chrono_days_in_zone(&entries, &edt()).expect("dates"),
            vec![d("2026-09-17")]
        );
    }

    /// A malformed `start_time` is an error naming the value, not a silently
    /// dropped day — a dropped day is a day nobody settles.
    #[test]
    fn an_unparseable_start_time_is_an_error_naming_the_value() {
        let entries = vec![chrono_entry(1, "not a timestamp")];
        let error = chrono_days_in_zone(&entries, &edt()).expect_err("unparseable");
        assert!(
            error.to_string().contains("not a timestamp"),
            "the message names the value: {error}"
        );
    }

    // -----------------------------------------------------------------------
    // C14 — the final adjustment to exactly 8h
    // -----------------------------------------------------------------------

    /// `< 0.01` on the absolute difference: a day already at 7.995h is left alone,
    /// and one at 7.98h is not.
    #[test]
    fn a_day_within_a_hundredth_of_eight_hours_is_left_alone() {
        let day = vec![Row::new("2026-09-15", "A", 7.995).build()];
        assert_eq!(hours(&adjust_to_eight_hours(&day, 4.0)), vec![7.995]);

        let short = vec![Row::new("2026-09-15", "A", 7.98).build()];
        assert_ne!(
            hours(&adjust_to_eight_hours(&short, 4.0)),
            vec![7.98],
            "a fiftieth short is past the tolerance and gets adjusted"
        );
    }

    /// The largest non-synthetic scalable entry absorbs the whole difference, and
    /// the others are untouched.
    #[test]
    fn the_largest_non_synthetic_scalable_entry_absorbs_the_difference() {
        let day = vec![
            Row::new("2026-09-15", "A", 2.0).build(),
            Row::new("2026-09-15", "B", 3.0).build(),
            Row::new("2026-09-15", "M", 1.0).meeting().build(),
        ];
        assert_eq!(
            hours(&adjust_to_eight_hours(&day, 4.0)),
            vec![2.0, 5.0, 1.0]
        );
    }

    /// `maxByOrNull` returns the **first** maximum. Rust's `max_by` returns the
    /// last, which would move the second 3.0 instead of the first — invisible on
    /// every day whose entries differ, and wrong on every day two of them tie.
    #[test]
    fn a_tie_between_the_two_largest_entries_is_broken_by_the_first() {
        let day = vec![
            Row::new("2026-09-15", "A", 3.0).build(),
            Row::new("2026-09-15", "B", 3.0).build(),
            Row::new("2026-09-15", "M", 1.0).meeting().build(),
        ];
        assert_eq!(
            hours(&adjust_to_eight_hours(&day, 4.0)),
            vec![4.0, 3.0, 1.0]
        );
    }

    /// Neither a meeting nor a manually fixed entry is a candidate, so a day
    /// holding one of each plus one ordinary entry moves only the ordinary one.
    #[test]
    fn meetings_and_manually_fixed_entries_never_scale() {
        let day = vec![
            Row::new("2026-09-15", "M", 3.0).meeting().build(),
            Row::new("2026-09-15", "F", 2.0).fixed().build(),
            Row::new("2026-09-15", "W", 1.0).build(),
        ];
        assert_eq!(
            hours(&adjust_to_eight_hours(&day, 4.0)),
            vec![3.0, 2.0, 3.0]
        );
    }

    /// A synthetic entry moves only when there is no real one to move.
    #[test]
    fn a_synthetic_entry_moves_only_when_no_real_entry_can() {
        let day = vec![
            Row::new("2026-09-15", "M", 4.0).meeting().build(),
            Row::new("2026-09-15", "F", 2.0).filler().build(),
        ];
        assert_eq!(hours(&adjust_to_eight_hours(&day, 4.0)), vec![4.0, 4.0]);
    }

    /// ...and then only inside what is left of the synthetic cap. Here the day is
    /// 3h short and the cap has 1h left, so the day ends at 6h and the under-8h
    /// warning is what reports it.
    #[test]
    fn a_synthetic_entry_moves_only_inside_the_remaining_cap() {
        let day = vec![
            Row::new("2026-09-15", "M", 3.0).meeting().build(),
            Row::new("2026-09-15", "F", 2.0).filler().build(),
        ];
        let adjusted = adjust_to_eight_hours(&day, 3.0);
        assert_eq!(hours(&adjusted), vec![3.0, 3.0]);
        assert_eq!(
            adjusted.iter().map(|a| a.normalized_hours).sum::<f64>(),
            6.0,
            "the cap wins over the 8h target"
        );
    }

    /// The cap already spent and the day short: nothing moves at all.
    #[test]
    fn a_short_day_whose_synthetic_cap_is_spent_stays_short() {
        let day = vec![
            Row::new("2026-09-15", "M", 3.0).meeting().build(),
            Row::new("2026-09-15", "F", 4.0).filler().build(),
        ];
        assert_eq!(hours(&adjust_to_eight_hours(&day, 4.0)), vec![3.0, 4.0]);
    }

    /// A borrowed entry counts against the same cap a filler does. A port checking
    /// only `is_filler` would find 2h of budget left here and lengthen the day.
    #[test]
    fn a_borrowed_entry_counts_against_the_synthetic_cap_exactly_as_a_filler_does() {
        let day = vec![
            Row::new("2026-09-15", "M", 2.0).meeting().build(),
            Row::new("2026-09-15", "F", 2.0).filler().build(),
            Row::new("2026-09-15", "B", 2.0)
                .borrowed("2026-09-14")
                .build(),
        ];
        assert_eq!(
            hours(&adjust_to_eight_hours(&day, 4.0)),
            vec![2.0, 2.0, 2.0]
        );
    }

    /// Nothing scalable at all — the day is left exactly as it arrived rather than
    /// having a meeting stretched.
    #[test]
    fn a_day_with_nothing_scalable_is_untouched() {
        let day = vec![
            Row::new("2026-09-15", "M", 3.0).meeting().build(),
            Row::new("2026-09-15", "N", 2.0).meeting().build(),
        ];
        assert_eq!(hours(&adjust_to_eight_hours(&day, 4.0)), vec![3.0, 2.0]);
    }

    /// An over-full day shrinks the largest **without** consulting the budget: the
    /// `diff > 0` guard is what makes the cap a floor on growth only. The remaining
    /// budget here is negative and the entry still moves.
    #[test]
    fn an_over_full_day_shrinks_the_largest_without_consulting_the_synthetic_cap() {
        let day = vec![
            Row::new("2026-09-15", "M", 4.0).meeting().build(),
            Row::new("2026-09-15", "F", 6.0).filler().build(),
        ];
        assert_eq!(hours(&adjust_to_eight_hours(&day, 4.0)), vec![4.0, 4.0]);
    }

    /// The adjustment itself is truncated to a quarter, so a gap smaller than a
    /// quarter cannot be closed and the day stays short of 8h. A port rounding here
    /// would report a tidy 8.0 the portal never receives.
    #[test]
    fn a_sub_quarter_gap_stays_open_because_the_adjustment_truncates() {
        let day = vec![
            Row::new("2026-09-15", "A", 2.0).build(),
            Row::new("2026-09-15", "B", 3.0).build(),
            Row::new("2026-09-15", "M", 2.9).meeting().build(),
        ];
        let adjusted = adjust_to_eight_hours(&day, 4.0);
        assert_eq!(hours(&adjusted), vec![2.0, 3.0, 2.9]);
        assert!(
            adjusted.iter().map(|a| a.normalized_hours).sum::<f64>() < 8.0,
            "the day is still short, by less than a quarter"
        );
    }

    /// `groupBy` then `.values.flatten()`: the output is grouped by date in
    /// first-encounter order, **not** in the input's interleaved order, and each
    /// day is adjusted against its own total.
    #[test]
    fn interleaved_days_come_back_grouped_by_date_in_first_encounter_order() {
        let actions = vec![
            Row::new("2026-09-15", "A", 3.0).build(),
            Row::new("2026-09-16", "B", 2.0).build(),
            Row::new("2026-09-15", "C", 3.0).build(),
        ];
        let adjusted = adjust_to_eight_hours(&actions, 4.0);
        let dates: Vec<NaiveDate> = adjusted.iter().map(|a| a.aggregate.date).collect();
        assert_eq!(
            dates,
            vec![d("2026-09-15"), d("2026-09-15"), d("2026-09-16")]
        );
        assert_eq!(hours(&adjusted), vec![5.0, 3.0, 8.0]);
    }

    #[test]
    fn an_empty_action_list_adjusts_to_an_empty_action_list() {
        assert!(adjust_to_eight_hours(&[], 4.0).is_empty());
    }

    // -----------------------------------------------------------------------
    // C15 — redistribution after an edit or a delete
    // -----------------------------------------------------------------------

    /// Scalable entries are scaled into what the fixed ones leave, proportionally.
    #[test]
    fn scalable_entries_scale_into_what_the_fixed_entries_leave() {
        let day = vec![
            Row::new("2026-09-15", "M", 2.0).meeting().build(),
            Row::new("2026-09-15", "A", 2.0).build(),
            Row::new("2026-09-15", "B", 6.0).build(),
        ];
        assert_eq!(hours(&renormalize_after_edit(&day)), vec![2.0, 1.5, 4.5]);
    }

    /// A residual of at least 0.125 goes to the largest scaled entry, and on a tie
    /// the stable `sortedByDescending` leaves the **earliest** of the maxima first —
    /// the same first-wins rule `maxByOrNull` has, reached another way.
    #[test]
    fn the_residual_goes_to_the_earliest_of_the_tied_maxima() {
        let day = vec![
            Row::new("2026-09-15", "M", 1.0).meeting().build(),
            Row::new("2026-09-15", "A", 1.0).build(),
            Row::new("2026-09-15", "B", 1.0).build(),
            Row::new("2026-09-15", "C", 1.0).build(),
        ];
        let result = renormalize_after_edit(&day);
        assert_eq!(hours(&result), vec![1.0, 2.5, 2.25, 2.25]);
        assert_eq!(result.iter().map(|a| a.normalized_hours).sum::<f64>(), 8.0);
    }

    /// Below an eighth of an hour the residual is left on the table and the day
    /// ends short. `>= 0.125`, so this is the open side of that boundary.
    #[test]
    fn a_residual_below_an_eighth_of_an_hour_is_left_on_the_table() {
        let day = vec![
            Row::new("2026-09-15", "M", 1.15).meeting().build(),
            Row::new("2026-09-15", "A", 4.0).build(),
        ];
        let result = renormalize_after_edit(&day);
        assert_eq!(hours(&result), vec![1.15, 6.75]);
        assert!(
            result.iter().map(|a| a.normalized_hours).sum::<f64>() < 8.0,
            "the tenth of an hour is not redistributed"
        );
    }

    /// The scale truncates rather than rounds, and so does the residual pass — a
    /// rounding port lands on 7.25 here and a truncating one on 7.0.
    #[test]
    fn the_scale_truncates_rather_than_rounds_even_after_the_residual_pass() {
        let day = vec![
            Row::new("2026-09-15", "M", 0.85).meeting().build(),
            Row::new("2026-09-15", "A", 4.0).build(),
        ];
        assert_eq!(hours(&renormalize_after_edit(&day)), vec![0.85, 7.0]);
    }

    /// `targetHours <= 0` is not a rounding case: with nothing left to scale into,
    /// every scalable entry is set to a flat 0.25 and the day ends **over** 8h.
    #[test]
    fn when_fixed_entries_already_reach_eight_hours_every_scalable_entry_drops_to_a_flat_quarter() {
        let day = vec![
            Row::new("2026-09-15", "M", 8.0).meeting().build(),
            Row::new("2026-09-15", "A", 3.0).build(),
            Row::new("2026-09-15", "B", 2.0).build(),
        ];
        let result = renormalize_after_edit(&day);
        assert_eq!(hours(&result), vec![8.0, 0.25, 0.25]);
        assert!(
            result.iter().map(|a| a.normalized_hours).sum::<f64>() > FULL_DAY_HOURS,
            "the day deliberately ends over 8h"
        );
    }

    /// A manually fixed entry holds its hours here exactly as a meeting does.
    #[test]
    fn a_manually_fixed_entry_holds_its_hours_through_the_redistribution() {
        let day = vec![
            Row::new("2026-09-15", "F", 5.0).fixed().build(),
            Row::new("2026-09-15", "A", 1.0).build(),
        ];
        assert_eq!(hours(&renormalize_after_edit(&day)), vec![5.0, 3.0]);
    }

    /// Nothing scalable: the list comes back as it went in, and the caller's redraw
    /// plus the warning is what tells the operator why.
    #[test]
    fn no_scalable_entry_means_the_list_comes_back_untouched() {
        let day = vec![
            Row::new("2026-09-15", "M", 3.0).meeting().build(),
            Row::new("2026-09-15", "F", 2.0).fixed().build(),
        ];
        assert_eq!(renormalize_after_edit(&day), day);
    }

    /// The `associateBy { it.aggregate }` trap, ported as written: the map is keyed
    /// by a **value**, so two actions sharing an aggregate collapse onto the last
    /// one that claimed the key. A port mapping the scaled entries back by index
    /// would leave the first at 2.0.
    #[test]
    fn two_actions_built_from_one_aggregate_collapse_onto_the_last_scaled_value() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0).normalized(2.0).build(),
            Row::new("2026-09-15", "A", 4.0).normalized(6.0).build(),
        ];
        assert_eq!(
            day[0].aggregate, day[1].aggregate,
            "the premise: the two actions share one aggregate value"
        );
        assert_eq!(hours(&renormalize_after_edit(&day)), vec![6.0, 6.0]);
    }

    /// Two actions whose aggregates differ by one field do **not** collapse, which
    /// is what makes the test above an assertion about equality rather than about
    /// the loop.
    #[test]
    fn two_actions_with_different_aggregates_keep_their_own_scaled_values() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0).normalized(2.0).build(),
            Row::new("2026-09-15", "B", 4.0).normalized(6.0).build(),
        ];
        assert_eq!(hours(&renormalize_after_edit(&day)), vec![2.0, 6.0]);
    }

    // -----------------------------------------------------------------------
    // The tail of prepareActions
    // -----------------------------------------------------------------------

    fn normalized(
        date: &str,
        devpro: &str,
        hours: f64,
        descriptions: &[&str],
    ) -> NormalizedAggregate {
        NormalizedAggregate {
            original: DayProjectAggregate {
                date: d(date),
                chrono_project: "Work - DevPro - Work".to_string(),
                total_hours: hours,
                descriptions: descriptions.iter().map(|t| (*t).to_string()).collect(),
                devpro_project_name: devpro.to_string(),
                billability: "Billable".to_string(),
                max_hours: None,
            },
            normalized_hours: hours,
            is_meeting: false,
        }
    }

    fn filler_entry(date: &str, devpro: &str, hours: f64) -> FillerEntry {
        FillerEntry {
            date: d(date),
            devpro_project_name: devpro.to_string(),
            task_title: "Internal work".to_string(),
            billability: "NonBillable".to_string(),
            hours,
        }
    }

    fn borrowed_entry(date: &str, source: &str, devpro: &str, hours: f64) -> BorrowedEntry {
        BorrowedEntry {
            date: d(date),
            source_date: d(source),
            devpro_project_name: devpro.to_string(),
            task_title: "Borrowed work".to_string(),
            billability: "Billable".to_string(),
            hours,
        }
    }

    /// Every name on every day resolves to the same id map.
    fn ids(days: &[&str], names: &[(&str, &str)]) -> IdsByDay {
        let mut by_day = HashMap::new();
        for day in days {
            let mut map = HashMap::new();
            for (name, id) in names {
                map.insert((*name).to_string(), (*id).to_string());
            }
            by_day.insert(d(day), map);
        }
        by_day
    }

    /// The three lists are built in order and the whole is then sorted by
    /// `(date, devproProjectName)`.
    #[test]
    fn the_three_kinds_of_proposal_are_sorted_by_date_then_project_name() {
        let actions = build_actions(
            &[normalized("2026-09-16", "Zeta", 4.0, &["Work"])],
            &[filler_entry("2026-09-15", "Alpha", 2.0)],
            &[borrowed_entry("2026-09-15", "2026-09-14", "Beta", 2.0)],
            &ids(
                &["2026-09-15", "2026-09-16"],
                &[("Zeta", "z"), ("Alpha", "a"), ("Beta", "b")],
            ),
            &[],
            8.0,
        )
        .expect("every name has an id");

        let order: Vec<(NaiveDate, &str)> = actions
            .iter()
            .map(|a| (a.aggregate.date, a.aggregate.devpro_project_name.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                (d("2026-09-15"), "Alpha"),
                (d("2026-09-15"), "Beta"),
                (d("2026-09-16"), "Zeta")
            ]
        );
    }

    /// The markers are the strings `clean_chrono_entry` keys its display off, so
    /// they are load-bearing rather than decorative; a borrowed entry also carries
    /// the day it came from.
    #[test]
    fn a_filler_carries_the_filler_marker_and_a_borrowed_entry_its_marker_and_source_date() {
        let actions = build_actions(
            &[],
            &[filler_entry("2026-09-15", "Alpha", 4.0)],
            &[borrowed_entry("2026-09-15", "2026-09-14", "Beta", 4.0)],
            &ids(&["2026-09-15"], &[("Alpha", "a"), ("Beta", "b")]),
            &[],
            8.0,
        )
        .expect("every name has an id");

        assert_eq!(actions[0].aggregate.chrono_project, "[filler]");
        assert!(actions[0].is_filler && !actions[0].is_borrowed);
        assert_eq!(actions[0].source_date, None);

        assert_eq!(actions[1].aggregate.chrono_project, "[borrowed]");
        assert!(actions[1].is_borrowed && !actions[1].is_filler);
        assert_eq!(actions[1].source_date, Some(d("2026-09-14")));
    }

    /// C12 at its primary site: the first description with the project suffix
    /// stripped, and `"Development work"` when there are none.
    #[test]
    fn a_real_aggregates_title_is_the_first_description_cleaned() {
        let actions = build_actions(
            &[
                normalized(
                    "2026-09-15",
                    "Alpha",
                    4.0,
                    &["Refactoring - Work - DevPro - Work"],
                ),
                normalized("2026-09-15", "Beta", 4.0, &[]),
            ],
            &[],
            &[],
            &ids(&["2026-09-15"], &[("Alpha", "a"), ("Beta", "b")]),
            &[],
            8.0,
        )
        .expect("every name has an id");
        assert_eq!(actions[0].task_title, "Refactoring");
        assert_eq!(actions[1].task_title, "Development work");
    }

    /// A day already holding a worklog for the project becomes an UPDATE carrying
    /// that worklog's id; a day that does not is a CREATE with none.
    #[test]
    fn an_existing_worklog_turns_the_proposal_into_an_update_carrying_its_id() {
        let existing = vec![(worklog("w-1", "a", "Anything"), d("2026-09-15"))];
        let actions = build_actions(
            &[
                normalized("2026-09-15", "Alpha", 4.0, &["Work"]),
                normalized("2026-09-15", "Beta", 4.0, &["Work"]),
            ],
            &[],
            &[],
            &ids(&["2026-09-15"], &[("Alpha", "a"), ("Beta", "b")]),
            &existing,
            8.0,
        )
        .expect("every name has an id");
        assert_eq!(actions[0].action, ActionType::Update);
        assert_eq!(actions[0].existing_worklog_id.as_deref(), Some("w-1"));
        assert_eq!(actions[1].action, ActionType::Create);
        assert_eq!(actions[1].existing_worklog_id, None);
    }

    /// The incumbent writes `projectIdMap[name]!!` here. A message naming the name
    /// and the day beats a panic, and under D2 the day is half the answer.
    #[test]
    fn a_name_with_no_id_for_that_day_is_an_error_naming_both_the_name_and_the_day() {
        let error = build_actions(
            &[normalized("2026-09-16", "Alpha", 4.0, &["Work"])],
            &[],
            &[],
            &ids(&["2026-09-15"], &[("Alpha", "a")]),
            &[],
            8.0,
        )
        .expect_err("the id map has no 2026-09-16");
        let message = error.to_string();
        assert!(message.contains("Alpha"), "names the project: {message}");
        assert!(message.contains("2026-09-16"), "names the day: {message}");
    }

    /// The sort is stable, so two proposals on one project keep the order the three
    /// lists put them in — the real aggregate before the filler.
    #[test]
    fn the_sort_is_stable_so_two_proposals_on_one_project_keep_their_order() {
        let actions = build_actions(
            &[normalized("2026-09-15", "Alpha", 4.0, &["Real work"])],
            &[filler_entry("2026-09-15", "Alpha", 4.0)],
            &[],
            &ids(&["2026-09-15"], &[("Alpha", "a")]),
            &[],
            8.0,
        )
        .expect("every name has an id");
        assert_eq!(actions[0].task_title, "Real work");
        assert!(actions[1].is_filler);
    }

    /// `String.compareTo` compares UTF-16 code units, so a supplementary character
    /// — whose leading surrogate is 0xD800 — sorts **before** U+FF3A. Code-point
    /// order, which `str::cmp` gives, is the opposite.
    #[test]
    fn the_project_name_sort_is_javas_utf16_order_and_not_code_point_order() {
        let astral = "\u{10000}A";
        let halfwidth = "\u{FF3A}B";
        assert!(
            astral > halfwidth,
            "the premise: Rust's own ordering puts the astral name second"
        );

        let actions = build_actions(
            &[
                normalized("2026-09-15", halfwidth, 4.0, &["Work"]),
                normalized("2026-09-15", astral, 4.0, &["Work"]),
            ],
            &[],
            &[],
            &ids(&["2026-09-15"], &[(halfwidth, "h"), (astral, "s")]),
            &[],
            8.0,
        )
        .expect("every name has an id");
        assert_eq!(actions[0].aggregate.devpro_project_name, astral);
        assert_eq!(actions[1].aggregate.devpro_project_name, halfwidth);
    }

    /// `build_actions` runs the C14 adjustment before it sorts, so what comes out
    /// already totals 8h for the day.
    #[test]
    fn build_actions_adjusts_the_day_to_eight_hours_before_sorting() {
        let actions = build_actions(
            &[
                normalized("2026-09-15", "Alpha", 2.0, &["Work"]),
                normalized("2026-09-15", "Beta", 3.0, &["Work"]),
            ],
            &[],
            &[],
            &ids(&["2026-09-15"], &[("Alpha", "a"), ("Beta", "b")]),
            &[],
            8.0,
        )
        .expect("every name has an id");
        assert_eq!(hours(&actions), vec![2.0, 6.0]);
    }

    // -----------------------------------------------------------------------
    // C25 — the draft table
    // -----------------------------------------------------------------------

    fn table_lines(actions: &[SettleAction]) -> Vec<String> {
        draft_table(actions)
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    /// Every column has a floor, so a table of short cells still lines up with one
    /// of long ones. The header is the floors laid end to end.
    #[test]
    fn every_column_is_at_least_its_floor_width() {
        let lines = table_lines(&[Row::new("2026-09-15", "A", 8.0)
            .chrono_project("CP")
            .title("T")
            .build()]);
        assert_eq!(
            lines[0],
            "Date       | Chrono Project | Chrono Entry | DevPro Project | DevPro Task | Type     | Hours         | Action"
        );
    }

    /// The separator is exactly as long as the header, and a row holding a
    /// non-ASCII cell is exactly that long too — because every width and every pad
    /// counts UTF-16 units. A port padding by `String::len` would push this row
    /// eleven bytes past the separator and break the column rules for every
    /// Cyrillic project name in the live config.
    #[test]
    fn a_row_with_a_non_ascii_cell_still_lines_up_with_the_header() {
        let wide = "Проект Альфа Бета";
        let lines = table_lines(&[Row::new("2026-09-15", wide, 8.0).build()]);
        let header = &lines[0];
        let separator = &lines[1];
        let row = &lines[2];

        assert_eq!(separator.chars().count(), utf16_len(header));
        assert!(separator.chars().all(|c| c == '-'));
        assert_eq!(utf16_len(row), utf16_len(header));
        assert_ne!(
            row.len(),
            utf16_len(row),
            "the premise: this row's byte length is not its display width"
        );
    }

    /// The Chrono-entry column caps at 50 units and the cell is then cut to 49 plus
    /// `…`. The width comes off the *untruncated* text, which is what makes the cap
    /// and the cut two separate rules.
    #[test]
    fn the_chrono_entry_column_caps_at_fifty_and_the_cell_is_cut_to_fortynine_plus_an_ellipsis() {
        let long = "A".repeat(60);
        let lines = table_lines(&[Row::new("2026-09-15", "P", 8.0)
            .descriptions(&[&long])
            .build()]);
        let expected_cell = format!("{}\u{2026}", "A".repeat(49));
        assert_eq!(utf16_len(&expected_cell), 50);
        assert!(
            lines[2].contains(&expected_cell),
            "the row holds the cut cell: {}",
            lines[2]
        );
        assert!(!lines[2].contains(&long), "and not the whole description");
    }

    /// An entry whose hours changed shows both figures joined by U+2192; one whose
    /// hours are within a hundredth of the original shows a single figure.
    #[test]
    fn an_entry_whose_hours_changed_shows_the_original_and_the_normalized_figure() {
        let changed = table_lines(&[Row::new("2026-09-15", "P", 8.0).original(6.0).build()]);
        assert!(changed[2].contains(" 6.00\u{2192} 8.00"), "{}", changed[2]);

        let unchanged = table_lines(&[Row::new("2026-09-15", "P", 8.0).build()]);
        assert!(!unchanged[2].contains('\u{2192}'), "{}", unchanged[2]);
        assert!(unchanged[2].contains(" 8.00"), "{}", unchanged[2]);
    }

    /// C25 in the hours column. Java's `%.2f` is HALF_UP on the shortest decimal
    /// representation, so 0.125 renders `0.13`; Rust's own `{:.2}` gives `0.12`.
    /// `originalHours` is the raw Chrono total and is not quantized, so this is the
    /// column where the two formatters actually meet.
    #[test]
    fn the_hours_column_rounds_half_up_the_way_java_does() {
        let lines = table_lines(&[Row::new("2026-09-15", "P", 0.125).build()]);
        assert!(lines[2].contains(" 0.13"), "{}", lines[2]);
        assert_eq!(
            format!("{:>5.2}", 0.125_f64),
            " 0.12",
            "the premise: Rust's own formatter disagrees"
        );
        assert_eq!(
            lines.last().expect("a total line"),
            "Total: 0.13 \u{2192} 0.13 hours, 1 entries"
        );
    }

    /// The total line sums both columns and counts the rows.
    #[test]
    fn the_total_line_sums_both_hour_columns_and_counts_the_entries() {
        let lines = table_lines(&[
            Row::new("2026-09-15", "A", 4.0).original(3.0).build(),
            Row::new("2026-09-15", "B", 4.0).original(2.0).build(),
        ]);
        assert_eq!(
            lines.last().expect("a total line"),
            "Total: 5.00 \u{2192} 8.00 hours, 2 entries"
        );
    }

    /// No rows at all is a header of minimum widths and a zero total, not a panic —
    /// the `?: floor` arm every caller rules out.
    #[test]
    fn an_empty_table_is_a_header_of_minimum_widths_and_a_zero_total() {
        let lines = table_lines(&[]);
        // Header, separator, no rows, separator again, total — the second separator
        // is pushed unconditionally, so an empty table is four lines and not three.
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].len(), lines[1].len());
        assert_eq!(lines[1], lines[2]);
        assert_eq!(lines[3], "Total: 0.00 \u{2192} 0.00 hours, 0 entries");
    }

    /// The type column reads off `is_meeting` alone, so a borrowed meeting is still
    /// `Meeting`, and the action column title-cases the enum name.
    #[test]
    fn the_type_and_action_columns_read_off_the_flags_they_are_named_for() {
        let lines = table_lines(&[Row::new("2026-09-15", "A", 4.0)
            .borrowed("2026-09-14")
            .meeting()
            .action(ActionType::Update)
            .build()]);
        assert!(lines[2].contains("Meeting"), "{}", lines[2]);
        assert!(lines[2].ends_with("Update"), "{}", lines[2]);
    }

    // -----------------------------------------------------------------------
    // The under-8h warning
    // -----------------------------------------------------------------------

    /// The glyph is U+26A0 U+FE0F followed by **two** spaces. The `project_ids`
    /// fallback warning is a bare U+26A0 and one space; normalising the two to one
    /// string breaks byte parity against the captures.
    #[test]
    fn the_under_eight_warning_carries_the_emoji_variation_selector_and_two_spaces() {
        let text = under_eight_warning(&[Row::new("2026-09-15", "A", 6.0).build()])
            .expect("a short day warns");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "");
        assert_eq!(
            lines[1],
            "\u{26A0}\u{FE0F}  WARNING: Some days don't reach 8h due to borrowed+filler cap:"
        );
        assert_eq!(lines[2], "  2026-09-15: 6.00h (need 2.00h more)");
    }

    /// A full day produces no warning at all, so the prompt keeps its plain form.
    #[test]
    fn a_full_day_produces_no_under_eight_warning() {
        assert_eq!(
            under_eight_warning(&[Row::new("2026-09-15", "A", 8.0).build()]),
            None
        );
    }

    /// The epsilon boundary `under_eight_days` applies: 7.99 is not short, 7.98 is.
    #[test]
    fn a_day_at_seven_ninety_nine_is_not_short_and_one_at_seven_ninety_eight_is() {
        assert_eq!(
            under_eight_warning(&[Row::new("2026-09-15", "A", 7.99).build()]),
            None
        );
        assert!(under_eight_warning(&[Row::new("2026-09-15", "A", 7.98).build()]).is_some());
    }

    /// Each short day gets its own line, in date order.
    #[test]
    fn every_short_day_gets_its_own_line_in_date_order() {
        let text = under_eight_warning(&[
            Row::new("2026-09-16", "A", 5.0).build(),
            Row::new("2026-09-15", "B", 7.0).build(),
        ])
        .expect("both days are short");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[2], "  2026-09-15: 7.00h (need 1.00h more)");
        assert_eq!(lines[3], "  2026-09-16: 5.00h (need 3.00h more)");
    }

    // -----------------------------------------------------------------------
    // C15 — the dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn the_five_day_letters_map_to_the_five_branches() {
        assert_eq!(day_choice(Some("a")), DayChoice::Approve);
        assert_eq!(day_choice(Some("e")), DayChoice::Edit);
        assert_eq!(day_choice(Some("d")), DayChoice::Delete);
        assert_eq!(day_choice(Some("s")), DayChoice::Skip);
        assert_eq!(day_choice(Some("c")), DayChoice::Cancel);
    }

    /// A bare Enter is **not** a re-prompt and not a skip: it abandons every
    /// remaining day with `Unknown option. Cancelled.` A port that looped on a typo
    /// is friendlier and is a different program.
    #[test]
    fn a_bare_enter_at_the_day_prompt_is_unknown_and_abandons_every_remaining_day() {
        assert_eq!(day_choice(Some("")), DayChoice::Unknown);
        assert_eq!(day_choice(Some("yes")), DayChoice::Unknown);
        assert_eq!(
            day_choice(Some("A")),
            DayChoice::Unknown,
            "the caller lowercases first"
        );
    }

    /// EOF and `c` reach the same branch, which is the one that prints `Cancelled.`
    /// without the `Unknown option.` in front of it.
    #[test]
    fn eof_and_the_letter_c_both_cancel_through_the_same_branch() {
        assert_eq!(day_choice(None), DayChoice::Cancel);
        assert_eq!(batch_choice(None), BatchChoice::Cancel);
    }

    /// The batch prompt has no edit, no delete and no skip. `e` is `Edit` at one
    /// prompt and `Unknown` — which cancels — at the other.
    #[test]
    fn the_batch_prompt_has_no_edit_delete_or_skip() {
        assert_eq!(batch_choice(Some("a")), BatchChoice::Approve);
        assert_eq!(batch_choice(Some("e")), BatchChoice::Unknown);
        assert_eq!(batch_choice(Some("d")), BatchChoice::Unknown);
        assert_eq!(batch_choice(Some("s")), BatchChoice::Unknown);
        assert_eq!(
            day_choice(Some("e")),
            DayChoice::Edit,
            "the premise: the other prompt takes it"
        );
    }

    /// Both prompt texts change when the warning fired — `[A]pprove` becomes
    /// `[A]pprove anyway`, so the operator is told what they are approving.
    #[test]
    fn both_prompts_say_approve_anyway_once_the_warning_has_fired() {
        assert_eq!(
            day_prompt(false),
            "\n[A]pprove / [E]dit / [D]elete / [S]kip / [C]ancel all: "
        );
        assert_eq!(
            day_prompt(true),
            "\n[A]pprove anyway / [E]dit / [D]elete / [S]kip / [C]ancel all: "
        );
        assert_eq!(batch_prompt(false), "\n[A]pprove / [C]ancel: ");
        assert_eq!(batch_prompt(true), "\n[A]pprove anyway / [C]ancel: ");
    }

    /// `read_choice` trims with Java's rules and lowercases; `read_entry` trims only.
    /// The missing `lowercase` is what makes `B` an invalid entry number rather than
    /// a way back.
    #[test]
    fn the_outer_prompt_lowercases_its_line_and_the_inner_one_does_not() {
        let mut outer = FakeConsole::typing(&["  A  "]);
        assert_eq!(read_choice(&mut outer).as_deref(), Some("a"));

        let mut inner = FakeConsole::typing(&["  B  "]);
        assert_eq!(read_entry(&mut inner).as_deref(), Some("B"));
    }

    // -----------------------------------------------------------------------
    // The entry-number prompt
    // -----------------------------------------------------------------------

    /// `b`, an empty line and EOF all go back — and the empty line is the opposite
    /// of what it means one prompt out, where it cancels the whole run.
    #[test]
    fn b_an_empty_line_and_eof_all_go_back_from_the_entry_prompt() {
        assert_eq!(entry_selection(Some("b"), 3), EntrySelection::GoBack);
        assert_eq!(entry_selection(Some(""), 3), EntrySelection::GoBack);
        assert_eq!(entry_selection(None, 3), EntrySelection::GoBack);
        assert_eq!(
            day_choice(Some("")),
            DayChoice::Unknown,
            "the premise: the same empty line cancels at the outer prompt"
        );
    }

    /// The listing is one-based and both ends are closed.
    #[test]
    fn the_entry_numbers_are_one_based_and_both_ends_are_closed() {
        assert_eq!(entry_selection(Some("1"), 3), EntrySelection::Chosen(0));
        assert_eq!(entry_selection(Some("3"), 3), EntrySelection::Chosen(2));
        assert_eq!(entry_selection(Some("0"), 3), EntrySelection::Invalid);
        assert_eq!(entry_selection(Some("4"), 3), EntrySelection::Invalid);
    }

    /// `toIntOrNull` reads every Unicode decimal digit here too.
    #[test]
    fn a_non_ascii_digit_selects_an_entry() {
        assert_eq!(
            entry_selection(Some("\u{0663}"), 3),
            EntrySelection::Chosen(2)
        );
    }

    /// The inner prompt does not lowercase, so `B` falls through to `toIntOrNull`,
    /// fails, and becomes `Invalid entry number.` rather than a way back.
    #[test]
    fn an_uppercase_b_is_not_a_way_back_but_an_invalid_entry_number() {
        assert_eq!(entry_selection(Some("B"), 3), EntrySelection::Invalid);
        assert_eq!(
            entry_selection(Some("b"), 3),
            EntrySelection::GoBack,
            "the premise: only the lowercase letter goes back"
        );
    }

    /// `toIntOrNull()?.minus(1)` is Kotlin `Int` arithmetic, which wraps. A
    /// `checked_sub` port would take a different branch and a plain `-` would panic
    /// in a debug build.
    #[test]
    fn int_min_wraps_to_int_max_and_is_then_rejected_as_out_of_range() {
        assert_eq!(
            entry_selection(Some("-2147483648"), 3),
            EntrySelection::Invalid
        );
        assert_eq!(entry_selection(Some("-1"), 3), EntrySelection::Invalid);
    }

    // -----------------------------------------------------------------------
    // C15 — edit and delete
    // -----------------------------------------------------------------------

    /// The two gates count different sets. A day of one meeting, one manually fixed
    /// entry and one ordinary entry refuses `[E]` — the redistribution would have
    /// nothing left to absorb it — and allows `[D]`.
    #[test]
    fn edit_refuses_a_day_whose_only_scalable_entry_is_one_while_delete_accepts_it() {
        let day = vec![
            Row::new("2026-09-15", "M", 2.0).meeting().build(),
            Row::new("2026-09-15", "F", 3.0).fixed().build(),
            Row::new("2026-09-15", "W", 3.0).build(),
        ];

        let mut edit_io = FakeConsole::new();
        assert_eq!(edit_entry(&day, &mut edit_io), day);
        assert!(
            edit_io.out_text().contains(
                "\u{2717} Cannot edit: need at least 2 work entries to redistribute hours."
            ),
            "{}",
            edit_io.out_text()
        );

        let mut delete_io = FakeConsole::typing(&["1"]);
        let after = delete_entry(&day, &mut delete_io);
        assert_eq!(after.len(), 2, "delete went ahead on the same day");
    }

    #[test]
    fn a_day_of_nothing_but_meetings_can_be_neither_edited_nor_deleted_from() {
        let day = vec![
            Row::new("2026-09-15", "M", 4.0).meeting().build(),
            Row::new("2026-09-15", "N", 4.0).meeting().build(),
        ];

        let mut edit_io = FakeConsole::new();
        assert_eq!(edit_entry(&day, &mut edit_io), day);
        assert!(
            edit_io
                .out_text()
                .contains("No editable entries (meetings cannot be edited).")
        );

        let mut delete_io = FakeConsole::new();
        assert_eq!(delete_entry(&day, &mut delete_io), day);
        assert!(
            delete_io
                .out_text()
                .contains("No deletable entries (meetings cannot be deleted).")
        );
    }

    #[test]
    fn delete_refuses_a_day_holding_a_single_deletable_entry() {
        let day = vec![
            Row::new("2026-09-15", "M", 4.0).meeting().build(),
            Row::new("2026-09-15", "W", 4.0).build(),
        ];
        let mut io = FakeConsole::new();
        assert_eq!(delete_entry(&day, &mut io), day);
        assert!(
            io.out_text()
                .contains("\u{2717} Cannot delete: need at least 2 work entries.")
        );
    }

    /// The chosen entry is marked manually fixed and its hours are **truncated** to
    /// a quarter — 2.6 becomes 2.5, not 2.75 — and the rest redistribute around it.
    #[test]
    fn edit_marks_the_chosen_entry_fixed_and_truncates_the_hours_that_were_typed() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0).build(),
            Row::new("2026-09-15", "B", 4.0).build(),
        ];
        let mut io = FakeConsole::typing(&["1", "2.6"]);
        let result = edit_entry(&day, &mut io);

        assert_eq!(hours(&result), vec![2.5, 5.5]);
        assert!(result[0].is_manually_fixed);
        assert!(!result[1].is_manually_fixed);
        assert!(
            io.out_text().contains("Current: 4.00h. New hours: "),
            "{}",
            io.out_text()
        );
    }

    /// A value under a quarter and one that will not parse at all share a message,
    /// and both leave the day exactly as it was.
    #[test]
    fn edit_refuses_a_value_below_a_quarter_and_an_unparseable_one_with_one_message() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0).build(),
            Row::new("2026-09-15", "B", 4.0).build(),
        ];

        for typed in ["0.1", "not a number", "0"] {
            let mut io = FakeConsole::typing(&["1", typed]);
            assert_eq!(
                edit_entry(&day, &mut io),
                day,
                "input {typed} changed the day"
            );
            assert!(
                io.out_text().contains("Invalid. Must be >= 0.25"),
                "input {typed}: {}",
                io.out_text()
            );
        }
    }

    /// The 8h floor `deleteEntry` does not have: the edit is refused, the minimum
    /// is named, and the day comes back untouched.
    #[test]
    fn edit_backs_out_when_the_result_would_fall_under_eight_hours_and_names_the_minimum() {
        let day = vec![
            Row::new("2026-09-15", "M", 0.77).meeting().build(),
            Row::new("2026-09-15", "A", 4.0).build(),
            Row::new("2026-09-15", "B", 4.0).build(),
        ];
        let mut io = FakeConsole::typing(&["1", "0.25"]);
        assert_eq!(edit_entry(&day, &mut io), day);
        let text = io.out_text();
        assert!(
            text.contains("\u{2717} Cannot set 0.25h \u{2014} would result in 7.77h total (< 8h)"),
            "{text}"
        );
        assert!(text.contains("  Minimum for this entry: 0.48h"), "{text}");
    }

    /// An empty hours line — and EOF, which is what a closed stdin gives — leaves
    /// the day alone without a message.
    #[test]
    fn an_empty_hours_line_or_eof_leaves_the_day_alone() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0).build(),
            Row::new("2026-09-15", "B", 4.0).build(),
        ];

        let mut empty = FakeConsole::typing(&["1", ""]);
        assert_eq!(edit_entry(&day, &mut empty), day);
        assert!(
            !empty.out_text().contains("Invalid"),
            "{}",
            empty.out_text()
        );

        let mut eof = FakeConsole::typing(&["1"]);
        assert_eq!(edit_entry(&day, &mut eof), day);
    }

    /// `b` at the entry prompt backs out before anything is asked about hours.
    #[test]
    fn going_back_from_the_entry_prompt_leaves_the_day_alone() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0).build(),
            Row::new("2026-09-15", "B", 4.0).build(),
        ];
        let mut io = FakeConsole::typing(&["b"]);
        assert_eq!(edit_entry(&day, &mut io), day);
        assert!(!io.out_text().contains("Current:"), "{}", io.out_text());
    }

    /// An out-of-range number is refused by name, and the listing marks a manually
    /// fixed entry with `*` — a marker only the edit listing then explains.
    #[test]
    fn the_listing_marks_a_manually_fixed_entry_with_a_star_and_rejects_a_bad_number() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0)
                .title("Fixed one")
                .fixed()
                .build(),
            Row::new("2026-09-15", "B", 2.0).title("Loose one").build(),
            Row::new("2026-09-15", "C", 2.0).title("Other one").build(),
        ];
        let mut io = FakeConsole::typing(&["9"]);
        assert_eq!(edit_entry(&day, &mut io), day);
        let text = io.out_text();
        assert!(text.contains("  1.* A: Fixed one (4.00h)"), "{text}");
        assert!(text.contains("  2.  B: Loose one (2.00h)"), "{text}");
        assert!(
            text.contains("  (* = manually fixed, won't scale)"),
            "{text}"
        );
        assert!(text.contains("Invalid entry number."), "{text}");
    }

    /// Delete removes exactly the chosen entry, says which, and redistributes.
    #[test]
    fn delete_removes_exactly_the_chosen_entry_and_redistributes_the_rest() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0).title("Keep me").build(),
            Row::new("2026-09-15", "B", 4.0).title("Drop me").build(),
        ];
        let mut io = FakeConsole::typing(&["2"]);
        let result = delete_entry(&day, &mut io);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].task_title, "Keep me");
        assert_eq!(result[0].normalized_hours, 8.0);
        assert!(
            io.out_text().contains("\u{2713} Deleted: Drop me"),
            "{}",
            io.out_text()
        );
    }

    /// Delete has **no** 8h floor: dropping an entry can leave the day short, the
    /// next redraw shows the warning, and `[A]pprove anyway` is the answer. Making
    /// this consistent with `editEntry` would remove a deliberate escape hatch.
    #[test]
    fn delete_may_leave_the_day_under_eight_hours_and_does_it_anyway() {
        let day = vec![
            Row::new("2026-09-15", "M", 0.77).meeting().build(),
            Row::new("2026-09-15", "A", 4.0).title("Keep me").build(),
            Row::new("2026-09-15", "B", 3.0).title("Drop me").build(),
        ];
        let mut io = FakeConsole::typing(&["2"]);
        let result = delete_entry(&day, &mut io);

        assert_eq!(hours(&result), vec![0.77, 7.0]);
        let total: f64 = result.iter().map(|a| a.normalized_hours).sum();
        assert!(total < FULL_DAY_HOURS, "the day is left at {total}h");
        assert!(
            under_eight_warning(&result).is_some(),
            "and the redraw will say so"
        );
    }

    /// The delete listing carries the same `*` marker and no explanation of it.
    #[test]
    fn the_delete_listing_marks_a_fixed_entry_but_does_not_explain_the_marker() {
        let day = vec![
            Row::new("2026-09-15", "A", 4.0)
                .title("Fixed one")
                .fixed()
                .build(),
            Row::new("2026-09-15", "B", 4.0).title("Loose one").build(),
        ];
        let mut io = FakeConsole::typing(&["b"]);
        assert_eq!(delete_entry(&day, &mut io), day);
        let text = io.out_text();
        assert!(text.contains("  1.* A: Fixed one (4.00h)"), "{text}");
        assert!(!text.contains("(* = manually fixed"), "{text}");
    }

    // -----------------------------------------------------------------------
    // C18 / C19 — building the two write bodies from an action
    // -----------------------------------------------------------------------

    /// `SettleCommand.kt:876-883`. Six fields are copied off the action and
    /// `expenseType` is the literal `"None"` — not the absent value the other four
    /// optionals take, and not the empty string. A port that left it `None` sends a
    /// `null` the portal has never been sent by this tool.
    #[test]
    fn a_create_request_copies_the_action_and_pins_expense_type_to_the_literal_none() {
        let action = Row::new("2026-09-18", "Delivery Practices", 0.5)
            .title("AI Heads Sync")
            .billability("NonBillable")
            .project_id("cf84fdca-4809-4678-98b1-2e7cc56537c0")
            .build();

        let request = create_request(&action);

        assert_eq!(request.worklog_date, "2026-09-18");
        assert_eq!(
            request.project_unique_id,
            "cf84fdca-4809-4678-98b1-2e7cc56537c0"
        );
        assert_eq!(request.task_title, "AI Heads Sync");
        assert_eq!(request.billability, "NonBillable");
        assert_eq!(request.duration, 0.5);
        assert_eq!(request.expense_type.as_deref(), Some("None"));
    }

    /// The duration is `normalizedHours` (`:881`), which is the whole point of the
    /// command: the aggregate's `totalHours` is what Chrono recorded and the
    /// normalized figure is what gets filed. A port that sent the total would post
    /// unnormalized hours and every other assertion here would still pass.
    #[test]
    fn a_create_request_sends_the_normalized_hours_and_not_the_aggregates_total() {
        let action = Row::new("2026-09-18", "Alpha", 6.0).normalized(2.5).build();
        assert_eq!(action.aggregate.total_hours, 6.0, "the premise");
        assert_eq!(create_request(&action).duration, 2.5);
        assert_eq!(
            update_request(
                &Row::new("2026-09-18", "Alpha", 6.0)
                    .normalized(2.5)
                    .existing("w-1")
                    .build()
            )
            .expect("an id is present")
            .duration,
            2.5,
            "and the update path reads the same field"
        );
    }

    /// C18's null set. `CreateWorklogRequest` has five optional fields and the
    /// settle path names exactly one of them, so the other four ride out as
    /// `null` — which is what the captured body shows and what the portal accepts.
    #[test]
    fn the_only_optional_field_a_create_request_fills_is_expense_type() {
        let request = create_request(&Row::new("2026-09-18", "Alpha", 1.0).build());
        assert_eq!(request.description, None);
        assert_eq!(request.overtime, None);
        assert_eq!(request.pif, None);
        assert_eq!(request.google_calendar_event_id, None);
        assert_eq!(request.expense_type.as_deref(), Some("None"));
    }

    /// `SettleCommand.kt:889-896`. The update body is the create body with
    /// `uniqueId` in front and no `googleCalendarEventId` at all — the two are
    /// separate request types on both sides and collapsing them into one would put
    /// a field on the wire that the incumbent's update never carries.
    #[test]
    fn an_update_request_carries_the_existing_worklog_id_and_otherwise_matches_the_create() {
        let action = Row::new("2026-09-15", "Beta", 3.25)
            .title("Velocitor NLP")
            .billability("Billable")
            .project_id("proj-9")
            .action(ActionType::Update)
            .existing("wl-4242")
            .build();

        let request = update_request(&action).expect("the action carries an id");

        assert_eq!(request.unique_id, "wl-4242");
        assert_eq!(request.worklog_date, "2026-09-15");
        assert_eq!(request.project_unique_id, "proj-9");
        assert_eq!(request.task_title, "Velocitor NLP");
        assert_eq!(request.billability, "Billable");
        assert_eq!(request.duration, 3.25);
        assert_eq!(request.expense_type.as_deref(), Some("None"));
    }

    /// The incumbent writes `existingWorklogId!!` inside the per-action `try`, so a
    /// missing id is one counted failure and not a crash. The port returns `Err` for
    /// the same reason — a `.unwrap()` here would abort the whole batch mid-way,
    /// which is the one outcome `applyAll` is built to avoid.
    #[test]
    fn an_update_request_without_an_existing_id_is_an_error_naming_the_field() {
        let action = Row::new("2026-09-15", "Beta", 1.0)
            .action(ActionType::Update)
            .build();
        assert_eq!(action.existing_worklog_id, None, "the premise");

        let error = update_request(&action).expect_err("no id, no request");
        assert_eq!(
            error.to_string(),
            "action.existingWorklogId must not be null"
        );
    }

    // -----------------------------------------------------------------------
    // C19 — applyAll's pre-flight filter
    // -----------------------------------------------------------------------

    /// `SettleCommand.kt:859-866`. The batch is validated as a whole before the
    /// first POST, so a day carrying one bad entry writes nothing at all. The stub
    /// is canned with a response nobody should fetch: if it is ever consumed the
    /// client wrote when it must not have.
    #[tokio::test]
    async fn a_zero_hour_create_aborts_the_batch_before_the_first_request() {
        let server = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200("true")]);
        let client = stub_client(&server.base_url);
        let batch = vec![
            Row::new("2026-09-15", "Alpha", 0.0).build(),
            Row::new("2026-09-15", "Beta", 4.0).build(),
        ];
        let mut io = FakeConsole::new();

        let error = apply_all(&batch, &client, &mut io)
            .await
            .expect_err("a non-positive entry aborts");

        assert_eq!(
            error.to_string(),
            "Found 1 entries with non-positive hours. Aborting."
        );
        assert!(
            server.seen().is_empty(),
            "nothing may reach the portal: {:?}",
            server.seen()
        );
        assert_eq!(
            io.err_text(),
            "\u{2717} Invalid hours: 2026-09-15 Alpha (0.0h)"
        );
        assert_eq!(
            io.out_text(),
            "",
            "and no tally line, because there is no tally"
        );
    }

    /// `:860` exempts `SKIP` explicitly. A skipped action is never sent, so its
    /// hours are not a claim about anything — and a port that validated the whole
    /// list would refuse to settle a day that holds one skip.
    #[tokio::test]
    async fn a_skip_at_zero_hours_is_exempt_and_does_not_abort_the_batch() {
        let server = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200("true")]);
        let client = stub_client(&server.base_url);
        let batch = vec![
            Row::new("2026-09-15", "Alpha", 0.0)
                .action(ActionType::Skip)
                .build(),
            Row::new("2026-09-15", "Beta", 4.0).build(),
        ];
        let mut io = FakeConsole::new();

        apply_all(&batch, &client, &mut io)
            .await
            .expect("the skip is exempt");

        let requests = server.requests();
        assert_eq!(requests.len(), 1, "only the Beta create");
        assert!(
            requests[0].target.ends_with("/worklog/create"),
            "{:?}",
            requests[0].target
        );
        assert!(
            io.out_text()
                .contains("Done! Created: 1, Updated: 0, Errors: 0"),
            "{}",
            io.out_text()
        );
    }

    /// Every offending entry gets its own line, the count in the abort message is
    /// their number, and the hours are rendered by `Double.toString` — so a
    /// negative whole number keeps its `.0`.
    #[tokio::test]
    async fn the_invalid_hours_lines_name_every_offender_and_print_java_doubles() {
        let server = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200("true")]);
        let client = stub_client(&server.base_url);
        let batch = vec![
            Row::new("2026-09-15", "Alpha", -1.0).build(),
            Row::new("2026-09-15", "Beta", 4.0).build(),
            Row::new("2026-09-16", "Gamma", 0.0)
                .action(ActionType::Update)
                .existing("w-1")
                .build(),
        ];
        let mut io = FakeConsole::new();

        let error = apply_all(&batch, &client, &mut io)
            .await
            .expect_err("two offenders");

        assert_eq!(
            error.to_string(),
            "Found 2 entries with non-positive hours. Aborting."
        );
        assert_eq!(
            io.err_text(),
            "\u{2717} Invalid hours: 2026-09-15 Alpha (-1.0h)\n\u{2717} Invalid hours: 2026-09-16 Gamma (0.0h)"
        );
        assert!(
            server.seen().is_empty(),
            "the UPDATE path is gated by the same filter"
        );
    }

    // -----------------------------------------------------------------------
    // C19 — applyAll against a stub portal
    // -----------------------------------------------------------------------

    /// The two write paths, in list order, each hitting its own endpoint with its
    /// own body. A port that sent both through one endpoint, or that reordered the
    /// batch, fails here rather than on the portal.
    #[tokio::test]
    async fn a_create_and_an_update_reach_the_portal_in_list_order_with_their_own_bodies() {
        let server = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200("true"),
            crate::api::stub::json_200("true"),
        ]);
        let client = stub_client(&server.base_url);
        let batch = vec![
            Row::new("2026-09-15", "Alpha", 2.5)
                .title("Alpha work")
                .project_id("p-a")
                .build(),
            Row::new("2026-09-15", "Beta", 5.5)
                .title("Beta work")
                .project_id("p-b")
                .action(ActionType::Update)
                .existing("wl-77")
                .build(),
        ];
        let mut io = FakeConsole::new();

        apply_all(&batch, &client, &mut io)
            .await
            .expect("both writes land");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].target.ends_with("/worklog/create"),
            "{}",
            requests[0].target
        );
        assert!(
            requests[1].target.ends_with("/worklog/update"),
            "{}",
            requests[1].target
        );
        assert!(
            requests[0].body.contains("\"taskTitle\":\"Alpha work\""),
            "{}",
            requests[0].body
        );
        assert!(
            requests[0].body.contains("\"duration\":2.5"),
            "{}",
            requests[0].body
        );
        assert!(
            requests[1].body.contains("\"uniqueId\":\"wl-77\""),
            "{}",
            requests[1].body
        );
        assert!(
            !requests[0].body.contains("uniqueId"),
            "a create carries no worklog id"
        );

        assert_eq!(
            io.out_text(),
            "\u{2713} Created: 2026-09-15 Alpha (2.5h)\n\u{2713} Updated: 2026-09-15 Beta (5.5h)\n\nDone! Created: 1, Updated: 1, Errors: 0"
        );
        assert_eq!(io.err_text(), "");
    }

    /// `ActionType.SKIP -> {}`: no call, no line, and no place in either tally.
    /// The server is canned with one response so a stray request would be captured.
    #[tokio::test]
    async fn a_skip_action_is_never_sent_and_appears_in_no_tally() {
        let server = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200("true")]);
        let client = stub_client(&server.base_url);
        let batch = vec![
            Row::new("2026-09-15", "Alpha", 4.0)
                .action(ActionType::Skip)
                .build(),
        ];
        let mut io = FakeConsole::new();

        apply_all(&batch, &client, &mut io)
            .await
            .expect("a skip cannot fail");

        assert!(server.seen().is_empty(), "{:?}", server.seen());
        assert_eq!(io.out_text(), "\nDone! Created: 0, Updated: 0, Errors: 0");
    }

    /// Each action sits in its own `try` (`:872-906`), so a rejected write is
    /// counted and the ones after it still go. A port that used `?` inside the loop
    /// would stop at the first rejection and leave the day half-written — the exact
    /// outcome the pre-flight filter exists to prevent, arrived at from the other
    /// side.
    #[tokio::test]
    async fn one_rejected_write_is_counted_and_the_batch_carries_on() {
        let server = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200("true"),
            crate::api::stub::response(500, "Internal Server Error", "text/plain", "boom"),
            crate::api::stub::json_200("true"),
        ]);
        let client = stub_client(&server.base_url);
        let batch = vec![
            Row::new("2026-09-15", "Alpha", 2.0).build(),
            Row::new("2026-09-15", "Beta", 3.0).build(),
            Row::new("2026-09-15", "Gamma", 3.0).build(),
        ];
        let mut io = FakeConsole::new();

        apply_all(&batch, &client, &mut io)
            .await
            .expect("the batch completes");

        assert_eq!(
            server.requests().len(),
            3,
            "the third write was still attempted"
        );
        assert_eq!(
            io.err_text(),
            "\u{2717} Failed: 2026-09-15 Beta - Server error (500): boom"
        );
        assert!(
            io.out_text()
                .contains("Done! Created: 2, Updated: 0, Errors: 1"),
            "{}",
            io.out_text()
        );
    }

    /// The `Err` from [`update_request`] is raised where the incumbent's
    /// `NullPointerException` would be — inside the per-action `try` — so it is one
    /// counted failure and the wire never sees it.
    #[tokio::test]
    async fn an_update_with_no_id_fails_only_that_action_and_never_reaches_the_wire() {
        let server = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200("true")]);
        let client = stub_client(&server.base_url);
        let batch = vec![
            Row::new("2026-09-15", "Alpha", 4.0)
                .action(ActionType::Update)
                .build(),
            Row::new("2026-09-15", "Beta", 4.0).build(),
        ];
        let mut io = FakeConsole::new();

        apply_all(&batch, &client, &mut io)
            .await
            .expect("the batch completes");

        let requests = server.requests();
        assert_eq!(requests.len(), 1, "only the create");
        assert!(
            requests[0].target.ends_with("/worklog/create"),
            "{}",
            requests[0].target
        );
        assert_eq!(
            io.err_text(),
            "\u{2717} Failed: 2026-09-15 Alpha - action.existingWorklogId must not be null"
        );
        assert!(
            io.out_text()
                .contains("Done! Created: 1, Updated: 0, Errors: 1"),
            "{}",
            io.out_text()
        );
    }

    /// `createWorklog` returns a `Boolean` and `applyAll` never looks at it:
    /// `created++` runs on the line after the call. `checkStatus` tests `== 200`, so
    /// a 201 comes back as `false` — and the incumbent still reports it as created.
    /// A port that gated the tally on the returned bool would print `Created: 0` for
    /// a write the portal accepted.
    #[tokio::test]
    async fn a_201_counts_as_created_because_the_incumbent_increments_before_it_looks() {
        let server = crate::api::stub::StubServer::start(vec![crate::api::stub::response(
            201,
            "Created",
            "application/json",
            "true",
        )]);
        let client = stub_client(&server.base_url);
        let batch = vec![Row::new("2026-09-15", "Alpha", 4.0).build()];
        let mut io = FakeConsole::new();

        apply_all(&batch, &client, &mut io)
            .await
            .expect("a 201 is not an error");

        assert_eq!(
            io.out_text(),
            "\u{2713} Created: 2026-09-15 Alpha (4.0h)\n\nDone! Created: 1, Updated: 0, Errors: 0"
        );
        assert_eq!(io.err_text(), "");
    }

    /// `:909` sits after the loop with nothing guarding it, so the tally is printed
    /// whatever happened — including a batch where every single write was rejected.
    #[tokio::test]
    async fn the_tally_line_is_printed_even_when_nothing_succeeded() {
        let server = crate::api::stub::StubServer::start(vec![crate::api::stub::response(
            500,
            "Internal Server Error",
            "text/plain",
            "down",
        )]);
        let client = stub_client(&server.base_url);
        let batch = vec![Row::new("2026-09-15", "Alpha", 4.0).build()];
        let mut io = FakeConsole::new();

        apply_all(&batch, &client, &mut io)
            .await
            .expect("a rejected write is not an abort");

        assert_eq!(io.out_text(), "\nDone! Created: 0, Updated: 0, Errors: 1");
        assert_eq!(
            io.err_text(),
            "\u{2717} Failed: 2026-09-15 Alpha - Server error (500): down"
        );
    }

    // -----------------------------------------------------------------------
    // C29 — the --json body
    // -----------------------------------------------------------------------

    /// kotlinx-serialization's pretty printer indents with four spaces per level.
    /// `serde_json::to_string_pretty` uses two, so a port that reached for it emits
    /// a body that differs from the incumbent's on every line but the first.
    #[test]
    fn the_json_is_pretty_printed_with_a_four_space_indent_like_kotlinx() {
        let body = json_body(&[Row::new("2026-09-18", "Alpha", 1.0).build()]).expect("json");
        let lines: Vec<&str> = body.lines().collect();

        assert_eq!(lines[0], "[");
        assert_eq!(lines[1], "    {");
        assert_eq!(lines[2], "        \"aggregate\": {");
        assert_eq!(lines[3], "            \"date\": \"2026-09-18\",");
        assert!(
            !body.contains("\n  \""),
            "a two-space indent would show here: {body}"
        );
    }

    /// `~/.cache/tt-devpro-rewrite/baseline/settle-json.out` opens `[` and closes
    /// `]` with the elements between, so an empty run is those two characters and a
    /// newline — not `[]` on one line, which is what a compact encoder gives.
    #[test]
    fn an_empty_action_list_is_an_empty_json_array() {
        assert_eq!(json_body(&[]).expect("json"), "[]");
    }

    /// Byte for byte against the first element of
    /// `~/.cache/tt-devpro-rewrite/baseline/settle-json.out`, captured from the
    /// pinned incumbent binary on 2026-09-18 data. Field order, the four-space
    /// indent, the one-string-per-line `descriptions` array, the `CREATE` spelling
    /// and the absence of `isBorrowed` / `isManuallyFixed` / `maxHours` are all one
    /// assertion, because on the wire they are one string.
    #[test]
    fn one_action_serializes_byte_for_byte_like_the_captured_baseline() {
        let action = Row::new("2026-09-18", "Delivery Practices", 0.5)
            .chrono_project("Practices - DevPro - Work")
            .title("AI Heads Sync")
            .billability("NonBillable")
            .meeting()
            .project_id("cf84fdca-4809-4678-98b1-2e7cc56537c0")
            .build();

        let expected = r#"[
    {
        "aggregate": {
            "date": "2026-09-18",
            "chronoProject": "Practices - DevPro - Work",
            "totalHours": 0.5,
            "descriptions": [
                "AI Heads Sync"
            ],
            "devproProjectName": "Delivery Practices",
            "billability": "NonBillable"
        },
        "normalizedHours": 0.5,
        "isMeeting": true,
        "isFiller": false,
        "taskTitle": "AI Heads Sync",
        "devproProjectId": "cf84fdca-4809-4678-98b1-2e7cc56537c0",
        "action": "CREATE"
    }
]"#;

        assert_eq!(json_body(&[action]).expect("json"), expected);
    }

    /// The three optional fields appear only once they hold something, and a
    /// borrowed entry is the case that carries all of them.
    #[test]
    fn a_borrowed_update_carries_the_fields_a_plain_create_omits() {
        let action = Row::new("2026-09-18", "Alpha", 2.0)
            .borrowed("2026-09-11")
            .fixed()
            .action(ActionType::Update)
            .existing("wl-1")
            .build();

        let body = json_body(&[action]).expect("json");
        assert!(body.contains("\"isBorrowed\": true"), "{body}");
        assert!(body.contains("\"sourceDate\": \"2026-09-11\""), "{body}");
        assert!(body.contains("\"existingWorklogId\": \"wl-1\""), "{body}");
        assert!(body.contains("\"isManuallyFixed\": true"), "{body}");
        assert!(body.contains("\"action\": \"UPDATE\""), "{body}");
    }

    // -----------------------------------------------------------------------
    // The two catch clauses
    // -----------------------------------------------------------------------

    /// `SettleCommand.kt:105-106`. An `ApiException` gets its own prefix and the
    /// message alone — the status code it carries is deliberately not printed.
    #[test]
    fn an_api_error_is_reported_under_its_own_prefix_with_the_message_alone() {
        let mut io = FakeConsole::new();
        report_failure(
            &anyhow::Error::new(ApiError {
                status_code: 503,
                message: "Server error: Service Unavailable".to_string(),
            }),
            &mut io,
        );

        assert_eq!(
            io.err_text(),
            "\u{2717} API Error: Server error: Service Unavailable"
        );
        assert!(
            !io.err_text().contains("503"),
            "the status code stays out of the line"
        );
        assert_eq!(io.out_text(), "", "both clauses write to stderr");
    }

    /// `:107-108`. Everything else takes the plain prefix, and the port prints the
    /// whole `anyhow` chain rather than the outermost message: for a failed HTTP
    /// call the outermost message is `requesting http://...` and the refusal that
    /// caused it is one level down.
    #[test]
    fn any_other_error_is_reported_with_the_whole_context_chain() {
        let mut io = FakeConsole::new();
        let error = anyhow!("connection refused")
            .context("requesting http://localhost:9247/api/time-entries");
        report_failure(&error, &mut io);

        assert_eq!(
            io.err_text(),
            "\u{2717} Error: requesting http://localhost:9247/api/time-entries: connection refused"
        );
    }

    /// The match is a downcast through the chain, not a look at the outermost
    /// error, so an `ApiError` that picked up context on the way out still reads as
    /// an API failure. The context is then dropped, because `:106` prints
    /// `e.message` and nothing else.
    #[test]
    fn an_api_error_under_context_keeps_the_api_prefix_and_drops_the_context() {
        let mut io = FakeConsole::new();
        let error = anyhow::Error::new(ApiError {
            status_code: 404,
            message: "Resource not found.".to_string(),
        })
        .context("fetching the normal view");
        report_failure(&error, &mut io);

        assert_eq!(io.err_text(), "\u{2717} API Error: Resource not found.");
    }

    // -----------------------------------------------------------------------
    // The orchestration, against two stub origins
    // -----------------------------------------------------------------------
    //
    // D2 is decided here rather than asserted: the three facets the plan fixed —
    // project ids resolved per day, one `normalView` per month the range spans, and
    // one filler budget per billing period — are all invisible to the pure layer
    // above, because each is a question about *which requests get made*. Two stubs,
    // canned in request order, are the only instrument that can see them.

    /// Midday UTC, so that every zone from UTC-11 to UTC+11 dates the entry to
    /// `day`. [`aggregator::aggregate`] reads `ZoneId.systemDefault()` inline and
    /// [`Settle::prepare_actions`] calls it, so an orchestration test cannot pass a
    /// fixed zone in the way `aggregate_in_zone`'s own tests do — it has to pick a
    /// wall time no ordinary offset can push across a date boundary.
    fn work_entry(id: i64, day: &str, project: &str, description: &str) -> ChronoTimeEntry {
        ChronoTimeEntry {
            id,
            description: Some(description.to_string()),
            start_time: format!("{day}T12:00:00+00:00"),
            end_time: None,
            duration: Some(3600),
            project: Some(ChronoProject {
                id: 1,
                name: project.to_string(),
                color: "#000".to_string(),
                aspect: None,
            }),
            aspect: None,
        }
    }

    fn chrono_body(entries: &[ChronoTimeEntry]) -> String {
        serde_json::to_string(entries).expect("a Chrono body")
    }

    fn user_body(unique_id: &str) -> String {
        serde_json::to_string(&CurrentUser {
            unique_id: unique_id.to_string(),
            full_name: "Yurii Buchchenko".to_string(),
            email: "yurii.buchchenko@dev.pro".to_string(),
        })
        .expect("a currentUser body")
    }

    fn projects_body(contact: &str, projects: &[(&str, &str)]) -> String {
        serde_json::to_string(&crate::model::AssignedProjectsResponse {
            unique_id: contact.to_string(),
            projects: projects
                .iter()
                .map(|(id, name)| crate::model::Project {
                    unique_id: (*id).to_string(),
                    short_name: (*name).to_string(),
                    is_internal: false,
                    is_favorite: false,
                })
                .collect(),
        })
        .expect("an assignedProjectsOnDate body")
    }

    /// One page holding the days given. The `date` is written the way the portal
    /// writes it — a full timestamp whose first ten characters are the date — so
    /// that [`detail_date`] is exercised rather than bypassed.
    fn normal_view_body(days: &[(&str, f64, Vec<WorklogDetail>)]) -> String {
        serde_json::to_string(&crate::model::NormalViewResponse {
            total_logged_hours: 0.0,
            total_expected_hours: 0.0,
            page_list: vec![crate::model::PageItem {
                contact_unique_id: "u-1".to_string(),
                full_name: "Yurii Buchchenko".to_string(),
                logged_hours: 0.0,
                expected_hours: 0.0,
                details_by_dates: days
                    .iter()
                    .map(|(date, hours, worklogs)| crate::model::DateDetails {
                        date: format!("{date}T00:00:00"),
                        logged_hours: *hours,
                        expected_hours: 8.0,
                        worklogs_details: worklogs.clone(),
                    })
                    .collect(),
            }],
        })
        .expect("a normalView body")
    }

    /// `chrono_project -> devpro_project`, everything billable, no fillers and no
    /// overrides. A filler would pull the RNG and the budget map into every one of
    /// these tests; the filler's own module owns that.
    fn settle_config(chrono_base: &str, mappings: &[(&str, &str)]) -> Config {
        Config {
            chrono_api: chrono_base.to_string(),
            mappings: mappings
                .iter()
                .map(
                    |(chrono_project, devpro_project)| crate::config::ProjectMapping {
                        chrono_project: (*chrono_project).to_string(),
                        devpro_project: (*devpro_project).to_string(),
                        billability: "Billable".to_string(),
                    },
                )
                .collect(),
            fillers: Vec::new(),
            overrides: Vec::new(),
            project_ids: HashMap::new(),
            max_synthetic_hours: 4.0,
        }
    }

    /// The head of [`dispatch`] with the two clients pointed at stubs instead of at
    /// the portal and at `~/.tt-cookie`, followed by the very
    /// [`Settle::run_chosen_mode`] call `dispatch` ends on. `dispatch` builds its
    /// clients from [`crate::api::portal::BASE_URL`] and the cookie file, so that
    /// construction cannot be driven from a test without a production seam this
    /// port does not have. The routing after it can be, and is: it is one method
    /// and this harness calls it, so what these tests pin about an arm they pin
    /// about production too.
    async fn run_settle(
        args: &SettleArgs,
        config: &Config,
        portal_base: &str,
        today: NaiveDate,
        io: &mut dyn Console,
    ) -> Result<()> {
        let chrono_client = ChronoClient::new(&config.chrono_api).expect("a Chrono client");
        let tt_client = stub_client(portal_base);
        let normalizer = TimeNormalizer::with_knowledge_base("/definitely/not/a/knowledge/base");
        let settle = Settle {
            args,
            config,
            chrono_client: &chrono_client,
            tt_client: &tt_client,
            normalizer: &normalizer,
            today,
        };
        // `dispatch`'s own routing, reached by calling the same method `dispatch`
        // calls — the four arms exist once, so a test that pins an arm here pins
        // the production one. This used to be a second verbatim copy of the chain,
        // and before that it stopped at the two read-only modes, which left the
        // `!io.present()` branches of the other two — the C7 contract the
        // project's CLAUDE.md states as "when stdout is not a TTY, settle prints
        // the readable --dry-run summary instead of prompting" — with no test
        // able to reach them.
        settle.run_chosen_mode(io).await
    }

    fn json_args(from: &str, to: &str) -> SettleArgs {
        SettleArgs {
            from: Some(d(from)),
            to: Some(d(to)),
            json: true,
            ..SettleArgs::default()
        }
    }

    /// D2, second facet. `SettleCommand.kt:440-446` fetches `normalView` once, for
    /// the range's **first day**, so every worklog in a later month is invisible.
    /// The port walks [`months_in_range`], which for a range straddling a month
    /// boundary is two requests with the first of each month as the period.
    #[tokio::test]
    async fn a_range_spanning_two_months_fetches_one_normal_view_for_each_of_them() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-08-31", "Alpha - DevPro - Work", "August work"),
                work_entry(2, "2026-09-01", "Alpha - DevPro - Work", "September work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-08-31", "2026-09-01"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("the run completes");

        let requests = portal.requests();
        assert_eq!(
            requests.len(),
            5,
            "{:?}",
            requests.iter().map(|r| &r.target).collect::<Vec<_>>()
        );
        assert!(
            requests[0].target.contains("period=2026-08-01"),
            "{}",
            requests[0].target
        );
        assert!(
            requests[1].target.contains("period=2026-09-01"),
            "{}",
            requests[1].target
        );
        assert_eq!(
            chrono.requests().len(),
            1,
            "one Chrono fetch for the whole range"
        );
    }

    /// D2, first facet. `:463-478` asks for the assignments held on `from` and
    /// resolves every name in the range against that one list. The port asks once
    /// per day that has proposals, each with that day's own `dateFrom`.
    #[tokio::test]
    async fn project_ids_are_resolved_once_per_day_and_each_day_asks_for_its_own_date() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-14", "Alpha - DevPro - Work", "Monday"),
                work_entry(2, "2026-09-15", "Alpha - DevPro - Work", "Tuesday"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("contact-77")),
            crate::api::stub::json_200(&projects_body("contact-77", &[("id-alpha", "Alpha")])),
            crate::api::stub::json_200(&projects_body("contact-77", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-09-14", "2026-09-15"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("the run completes");

        let requests = portal.requests();
        assert_eq!(requests.len(), 4);
        assert!(
            requests[2]
                .target
                .starts_with("/contact/contact-77/assignedProjectsOnDate"),
            "the contact id comes from currentUser and goes in the path: {}",
            requests[2].target
        );
        assert!(
            requests[2].target.contains("dateFrom=2026-09-14"),
            "{}",
            requests[2].target
        );
        assert!(
            requests[3].target.contains("dateFrom=2026-09-15"),
            "{}",
            requests[3].target
        );
        let _ = chrono.requests();
    }

    /// The bug D2's first facet was written against, reproduced as a test: a project
    /// assigned only from the second day of the range. Measured against the
    /// incumbent, `--from 2026-08-01 --to 2026-08-15 --json` printed nothing but
    /// `✗ Error: DevPro project 'Inveniam SOW #5' not found.` for exactly this
    /// shape. The premise assertion is the other half — the union against day one's
    /// list is still an error, so the fix is the per-day split and not a widened
    /// lookup.
    #[tokio::test]
    async fn a_project_assigned_only_on_the_later_day_resolves_against_that_days_list() {
        let day_one_projects = vec![crate::model::Project {
            unique_id: "id-alpha".to_string(),
            short_name: "Alpha".to_string(),
            is_internal: false,
            is_favorite: false,
        }];
        assert!(
            aggregator::resolve_project_ids(
                &["Alpha".to_string(), "Beta".to_string()],
                &day_one_projects,
                &HashMap::new()
            )
            .is_err(),
            "the premise: resolving the range's union against the first day's list fails"
        );

        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-14", "Alpha - DevPro - Work", "Alpha day"),
                work_entry(2, "2026-09-15", "Beta - DevPro - Work", "Beta day"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-beta", "Beta")])),
        ]);
        let config = settle_config(
            &chrono.base_url,
            &[
                ("Alpha - DevPro - Work", "Alpha"),
                ("Beta - DevPro - Work", "Beta"),
            ],
        );
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-09-14", "2026-09-15"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("each day resolves against its own assignment list");

        let body = io.out_text();
        assert!(body.contains("\"devproProjectId\": \"id-alpha\""), "{body}");
        assert!(body.contains("\"devproProjectId\": \"id-beta\""), "{body}");
        let _ = (portal.requests(), chrono.requests());
    }

    /// D2's second facet in its consequence. The 09-01 worklog exists only in the
    /// September `normalView` response, so a port that fetched the range's first
    /// month alone would never see it and would propose a CREATE against a day that
    /// already holds a worklog for that project — a duplicate in the system of
    /// record, not a crash.
    #[tokio::test]
    async fn a_worklog_in_the_ranges_second_month_turns_its_proposal_into_an_update() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-08-31", "Alpha - DevPro - Work", "August work"),
                work_entry(2, "2026-09-01", "Alpha - DevPro - Work", "September work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[("2026-08-31", 0.0, Vec::new())])),
            crate::api::stub::json_200(&normal_view_body(&[(
                "2026-09-01",
                4.0,
                vec![worklog(
                    "wl-september",
                    "id-alpha",
                    "Something already filed",
                )],
            )])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-08-31", "2026-09-01"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("the run completes");

        let actions: Vec<SettleAction> =
            serde_json::from_str(&io.out_text()).expect("the JSON body parses back");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].aggregate.date, d("2026-08-31"));
        assert_eq!(actions[0].action, ActionType::Create);
        assert_eq!(actions[0].existing_worklog_id, None);
        assert_eq!(actions[1].aggregate.date, d("2026-09-01"));
        assert_eq!(actions[1].action, ActionType::Update);
        assert_eq!(
            actions[1].existing_worklog_id.as_deref(),
            Some("wl-september")
        );
        let _ = (portal.requests(), chrono.requests());
    }

    /// The fallback warning is printed once for the whole run even though the
    /// resolution now happens per day. The incumbent prints one line per fallback
    /// over the whole range; resolving per day without the dedup would print the
    /// same line once per day the project appears on, which for a 45-day scan is a
    /// wall of identical warnings around a real one.
    #[tokio::test]
    async fn the_fallback_warning_for_one_project_is_printed_once_however_many_days_it_names() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-14", "Alpha - DevPro - Work", "Monday"),
                work_entry(2, "2026-09-15", "Alpha - DevPro - Work", "Tuesday"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[])),
            crate::api::stub::json_200(&projects_body("u-1", &[])),
        ]);
        let mut config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        config
            .project_ids
            .insert("Alpha".to_string(), "configured-alpha".to_string());
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-09-14", "2026-09-15"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("a configured id keeps the run alive");

        let warnings = io
            .err_text()
            .matches("\u{26A0} 'Alpha' is not in your assigned projects")
            .count();
        assert_eq!(warnings, 1, "two days, one warning:\n{}", io.err_text());
        assert!(
            io.out_text()
                .contains("\"devproProjectId\": \"configured-alpha\""),
            "{}",
            io.out_text()
        );
        let _ = (portal.requests(), chrono.requests());
    }

    /// C7. Every progress line in `--json` mode goes to stderr, so stdout is the
    /// JSON and nothing else — a port that echoed `Fetching Chrono data…` to stdout
    /// makes the output unparseable for the automation the flag exists for.
    #[tokio::test]
    async fn in_json_mode_stdout_holds_only_the_json_and_the_progress_goes_to_stderr() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Alpha - DevPro - Work", "Work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-09-15", "2026-09-15"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("the run completes");

        let parsed: serde_json::Value =
            serde_json::from_str(&io.out_text()).expect("stdout is exactly one JSON document");
        assert_eq!(parsed.as_array().expect("an array").len(), 1);
        assert!(
            io.err_text()
                .contains("Fetching Chrono data (2026-09-15 to 2026-09-15)..."),
            "{}",
            io.err_text()
        );
        let _ = (portal.requests(), chrono.requests());
    }

    /// `--dry-run` reads and never writes. That assertion is on the method of every
    /// request that reached the stub, which is the only statement about writing
    /// that stays true however the summary is rendered.
    ///
    /// The second assertion is what separates this mode from `--json`, and it is
    /// deliberately at the mode level rather than the rendering level. `Deep work`
    /// is the entry's description, so it appears in the JSON payload too, and
    /// routing `DryRun` to `run_json_mode` used to satisfy everything else here.
    /// What cannot be true of both modes is the shape of the whole of stdout:
    /// `--json` emits exactly one JSON document and `--dry-run` emits a table for a
    /// human, so the pin is "this does not parse as JSON" — which still says
    /// nothing about which columns the summary has, or in what order.
    #[tokio::test]
    async fn a_dry_run_issues_reads_only_and_renders_the_day_summary() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Alpha - DevPro - Work", "Deep work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let args = SettleArgs {
            from: Some(d("2026-09-15")),
            to: Some(d("2026-09-15")),
            dry_run: true,
            ..SettleArgs::default()
        };
        let mut io = FakeConsole::new();

        run_settle(&args, &config, &portal.base_url, d("2026-09-20"), &mut io)
            .await
            .expect("the run completes");

        let requests = portal.requests();
        assert!(
            requests.iter().all(|r| r.method == "GET"),
            "{:?}",
            requests
                .iter()
                .map(|r| (&r.method, &r.target))
                .collect::<Vec<_>>()
        );
        assert!(io.out_text().contains("Deep work"), "{}", io.out_text());
        assert!(
            serde_json::from_str::<serde_json::Value>(&io.out_text()).is_err(),
            "--dry-run's stdout is a summary for a human, not a JSON document: {}",
            io.out_text()
        );
        let _ = chrono.requests();
    }

    /// `:418-421`. An empty Chrono answer ends `prepareActions` before the portal is
    /// asked anything at all — so a day with no tracked time costs one request, not
    /// five, and `--json` still emits a well-formed empty array.
    #[tokio::test]
    async fn an_empty_chrono_answer_stops_before_the_portal_is_asked_anything() {
        let chrono = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200("[]")]);
        let portal = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(
            &normal_view_body(&[]),
        )]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-09-15", "2026-09-15"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("the run completes");

        assert_eq!(io.out_text(), "[]");
        assert!(portal.seen().is_empty(), "{:?}", portal.seen());
        assert!(
            io.err_text()
                .contains("No entries found in Chrono for this period."),
            "{}",
            io.err_text()
        );
        let _ = chrono.requests();
    }

    /// C10, `Aggregator.kt:47`. The filter the project's own CLAUDE.md calls
    /// "unmapped Chrono projects are silently skipped" is this one and not the
    /// mapping lookup: a project whose name does not end in `DevPro - Work` never
    /// reaches the lookup at all. The portal is still never asked anything, because
    /// the aggregate list is empty before the first read.
    #[tokio::test]
    async fn a_chrono_project_outside_devpro_work_is_dropped_silently_and_costs_no_portal_request()
    {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Reading - Health - Personal", "A book"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(
            &normal_view_body(&[]),
        )]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-09-15", "2026-09-15"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("a non-DevPro project is not an error");

        assert_eq!(io.out_text(), "[]");
        assert!(portal.seen().is_empty(), "{:?}", portal.seen());
        assert!(
            io.err_text().contains("No work entries to process"),
            "{}",
            io.err_text()
        );
        assert!(
            !io.err_text().contains("Reading"),
            "the name is never mentioned: {}",
            io.err_text()
        );
        let _ = chrono.requests();
    }

    /// The other half, and the one this test module got wrong first time round: a
    /// project that **is** a DevPro Work project and has no mapping is not skipped
    /// at all. `Aggregator.kt:69` is a bare `error(...)`, so the run dies naming the
    /// project, offering the YAML to paste and listing what is configured — because
    /// the alternative is a day quietly settling short by however many hours that
    /// project held.
    #[tokio::test]
    async fn a_devpro_work_project_with_no_mapping_stops_the_run_and_lists_what_is_configured() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Gamma - DevPro - Work", "Unmapped"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(
            &normal_view_body(&[]),
        )]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new();

        let error = run_settle(
            &json_args("2026-09-15", "2026-09-15"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect_err("an unmapped DevPro Work project ends the run");

        let text = format!("{error:#}");
        assert!(
            text.starts_with("Chrono project 'Gamma - DevPro - Work' has no mapping in config."),
            "{text}"
        );
        assert!(
            text.contains("chrono_project: \"Gamma - DevPro - Work\""),
            "{text}"
        );
        assert!(
            text.contains("Currently configured projects:\n  - Alpha - DevPro - Work"),
            "{text}"
        );
        assert_eq!(
            io.out_text(),
            "",
            "nothing is emitted on stdout for a failed run"
        );
        assert!(
            portal.seen().is_empty(),
            "the failure precedes every portal read"
        );
        let _ = chrono.requests();
    }

    /// C17, `:205`. `getCurrentUser` is called before the month loop and its answer
    /// is thrown away — the call exists so that a dead session fails in one request
    /// instead of after forty-five days of months have been fetched. A port that
    /// moved it down to where the contact id is actually needed would pass every
    /// other test here and lose the early failure.
    #[tokio::test]
    async fn the_scan_asks_who_you_are_before_it_fetches_a_single_month() {
        let chrono = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&chrono_body(&[work_entry(
                1,
                "2026-09-18",
                "Alpha - DevPro - Work",
                "Friday work",
            )])),
            crate::api::stub::json_200(&chrono_body(&[work_entry(
                2,
                "2026-09-18",
                "Alpha - DevPro - Work",
                "Friday work",
            )])),
        ]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let args = SettleArgs {
            json: true,
            ..SettleArgs::default()
        };
        let mut io = FakeConsole::new();

        run_settle(&args, &config, &portal.base_url, d("2026-09-20"), &mut io)
            .await
            .expect("the scan completes");

        let requests = portal.requests();
        assert_eq!(
            requests[0].target, "/contact/currentUser",
            "before any month"
        );
        assert!(
            requests[1].target.contains("period=2026-08-01"),
            "{}",
            requests[1].target
        );
        assert!(
            requests[2].target.contains("period=2026-09-01"),
            "{}",
            requests[2].target
        );

        let chrono_requests = chrono.requests();
        assert_eq!(chrono_requests.len(), 2, "the scan's fetch, then the day's");
        assert!(
            chrono_requests[0].target.contains("start_date=2026-08-06"),
            "forty-five days before 2026-09-20: {}",
            chrono_requests[0].target
        );
        assert!(
            chrono_requests[0].target.contains("end_date=2026-09-20"),
            "C2's padding — the cutoff is 09-19 and the fetch runs to 09-20: {}",
            chrono_requests[0].target
        );

        let actions: Vec<SettleAction> =
            serde_json::from_str(&io.out_text()).expect("the JSON body parses back");
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].aggregate.date, d("2026-09-18"));
    }

    /// C2's padding on the explicit-range path. `SettleCommand.kt:421` fetches to
    /// `to.plusDays(1)` and the low end is not padded, because the two bounds do
    /// different jobs: the fetch runs on the UTC axis and every entry is then
    /// re-dated to its local day, so a late local evening filed under the next UTC
    /// day still arrives and a morning one was never at risk. A port that made the
    /// two bounds agree loses the last evening of every range — silently, because
    /// the day simply settles short.
    ///
    /// The scan path has its own padding, asserted in
    /// [`tests::the_scan_asks_who_you_are_before_it_fetches_a_single_month`]; this
    /// is the other of the two sites and they are not the same line of code.
    #[tokio::test]
    async fn the_explicit_range_fetch_pads_the_end_date_by_a_day_and_leaves_the_start_alone() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Alpha - DevPro - Work", "Work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new();

        run_settle(
            &json_args("2026-09-15", "2026-09-15"),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("the run completes");

        let requests = chrono.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].target.contains("start_date=2026-09-15"),
            "the low end is the range's own start: {}",
            requests[0].target
        );
        assert!(
            requests[0].target.contains("end_date=2026-09-16"),
            "the high end is padded by one UTC day: {}",
            requests[0].target
        );
        let _ = portal.requests();
    }

    /// C7 on the batch surface (`:1925`). With no console there is nothing to
    /// answer `[A]pprove / [C]ancel:`, so the readable summary stands in. The
    /// failure this pins is the one the branch's own comment names: without it the
    /// run falls through `read_line()` → `None` → `Cancelled.`, which reads like a
    /// decision somebody made rather than a machine that was never asked.
    #[tokio::test]
    async fn a_batch_run_with_no_console_prints_the_summary_instead_of_prompting() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Alpha - DevPro - Work", "Deep work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        // Neither `--json` nor `--dry-run`: this is the surface that would otherwise
        // write, which is what makes the absent console load-bearing.
        let args = SettleArgs {
            from: Some(d("2026-09-15")),
            to: Some(d("2026-09-15")),
            ..SettleArgs::default()
        };
        let mut io = FakeConsole::new().absent();

        run_settle(&args, &config, &portal.base_url, d("2026-09-20"), &mut io)
            .await
            .expect("the run completes");

        assert!(io.out_text().contains("Deep work"), "{}", io.out_text());
        assert!(
            !io.out_text().contains("[A]pprove"),
            "nothing may prompt a console that is not there: {}",
            io.out_text()
        );
        assert!(
            !io.out_text().contains("Cancelled."),
            "an unasked question is not a cancellation: {}",
            io.out_text()
        );
        let requests = portal.requests();
        assert!(
            requests.iter().all(|r| r.method == "GET"),
            "{:?}",
            requests
                .iter()
                .map(|r| (&r.method, &r.target))
                .collect::<Vec<_>>()
        );
        let _ = chrono.requests();
    }

    /// The batch prompt with a console actually present — the case the test above
    /// deliberately does not cover, and which nothing covered at all.
    ///
    /// Every piece the glue is made of is tested on its own: `batch_prompt`,
    /// `read_choice`, `batch_choice` and `apply_all`. The wiring between them —
    /// `run_batch_mode`'s `match` on `batch_choice`, the incumbent's `when` at
    /// `SettleCommand.kt:176-180` — was executed by no test, so any of the three
    /// outcomes could have been attached to the wrong branch. These three tests are
    /// the absent-console fixture with a typed answer in place of the absent
    /// console, and one more canned portal answer where a write is expected.
    ///
    /// `c` is the answer that must not write. The killing assertion is the one on
    /// stdout rather than the one on the stub's methods: a `Cancel` arm wired to
    /// `apply_all` would POST to a server that has already served its three canned
    /// answers and exited, so the write is refused rather than captured, and
    /// `apply_all` counts a refusal instead of propagating it. What such an arm
    /// cannot do is leave `Cancelled.` as the last line of stdout.
    #[tokio::test]
    async fn the_batch_prompt_cancels_on_c_without_reaching_the_write_path() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Alpha - DevPro - Work", "Deep work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let args = SettleArgs {
            from: Some(d("2026-09-15")),
            to: Some(d("2026-09-15")),
            ..SettleArgs::default()
        };
        let mut io = FakeConsole::typing(&["c"]);

        run_settle(&args, &config, &portal.base_url, d("2026-09-20"), &mut io)
            .await
            .expect("the run completes");

        let out = io.out_text();
        assert!(
            out.contains("[A]pprove / [C]ancel: "),
            "the prompt is the point of this branch: {out}"
        );
        assert!(
            out.ends_with("\nCancelled."),
            "`c` ends the run on the cancel line with nothing after it: {out}"
        );
        assert!(
            !out.contains("Unknown option."),
            "`c` is a known answer: {out}"
        );
        assert!(
            !out.contains("Done! Created:"),
            "a cancelled batch writes nothing and so tallies nothing: {out}"
        );
        let requests = portal.requests();
        assert!(
            requests.iter().all(|r| r.method == "GET"),
            "{:?}",
            requests
                .iter()
                .map(|r| (&r.method, &r.target))
                .collect::<Vec<_>>()
        );
        let _ = chrono.requests();
    }

    /// `SettleCommand.kt:179`, the `else` arm — an answer that is neither `a` nor
    /// `c`. `batch_choice` maps it to `Unknown`, which prints its own line and,
    /// like `Cancel`, writes nothing. The difference between the two is the whole
    /// of what this pins, and since both end in `Cancelled.` the assertion is on
    /// the entire last line, not a suffix.
    #[tokio::test]
    async fn the_batch_prompt_treats_an_unknown_letter_as_its_own_cancellation() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Alpha - DevPro - Work", "Deep work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let args = SettleArgs {
            from: Some(d("2026-09-15")),
            to: Some(d("2026-09-15")),
            ..SettleArgs::default()
        };
        let mut io = FakeConsole::typing(&["x"]);

        run_settle(&args, &config, &portal.base_url, d("2026-09-20"), &mut io)
            .await
            .expect("the run completes");

        let out = io.out_text();
        assert!(
            out.ends_with("\nUnknown option. Cancelled."),
            "an unknown letter says so before it cancels: {out}"
        );
        assert!(
            !out.contains("Done! Created:"),
            "and it writes nothing either: {out}"
        );
        let requests = portal.requests();
        assert!(
            requests.iter().all(|r| r.method == "GET"),
            "{:?}",
            requests
                .iter()
                .map(|r| (&r.method, &r.target))
                .collect::<Vec<_>>()
        );
        let _ = chrono.requests();
    }

    /// `a` on the batch prompt, the one answer that writes — the branch D4 keeps
    /// out of every real run, driven here against a stub process and never a
    /// portal.
    ///
    /// The stdout assertions come **before** `portal.requests()` on purpose:
    /// `requests` joins the server thread and blocks until all four canned answers
    /// have been consumed, so an `Approve` arm that stopped writing would hang here
    /// rather than fail. The tally line fails it first.
    #[tokio::test]
    async fn approving_the_batch_prompt_posts_the_worklog_it_drew() {
        let chrono =
            crate::api::stub::StubServer::start(vec![crate::api::stub::json_200(&chrono_body(&[
                work_entry(1, "2026-09-15", "Alpha - DevPro - Work", "Deep work"),
            ]))]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
            crate::api::stub::json_200("true"),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let args = SettleArgs {
            from: Some(d("2026-09-15")),
            to: Some(d("2026-09-15")),
            ..SettleArgs::default()
        };
        let mut io = FakeConsole::typing(&["a"]);

        run_settle(&args, &config, &portal.base_url, d("2026-09-20"), &mut io)
            .await
            .expect("the run completes");

        let out = io.out_text();
        assert!(
            !out.contains("Cancelled."),
            "`a` is not a cancellation: {out}"
        );
        assert!(
            out.ends_with("\nDone! Created: 1, Updated: 0, Errors: 0"),
            "one create, tallied: {out}"
        );
        assert_eq!(io.err_text(), "", "nothing was rejected");

        let requests = portal.requests();
        assert_eq!(requests.len(), 4, "three reads and the one write");
        assert_eq!(requests[3].method, "POST", "{:?}", requests[3].target);
        assert!(
            requests[3].target.ends_with("/worklog/create"),
            "{}",
            requests[3].target
        );
        let body = &requests[3].body;
        assert!(
            body.contains("\"worklogDate\":\"2026-09-15\""),
            "the day the table drew: {body}"
        );
        assert!(
            body.contains("\"projectUniqueId\":\"id-alpha\""),
            "the id resolved from the assigned-projects list: {body}"
        );
        assert!(
            body.contains("\"duration\":8.0"),
            "the hours that were posted are the normalized ones the table showed, \
             not the one tracked hour Chrono returned: {body}"
        );
        assert!(
            !body.contains("\"uniqueId\""),
            "a create carries no worklog id: {body}"
        );
        let _ = chrono.requests();
    }

    /// C7 on the day-by-day surface (`:1959`), the default mode — no flags and no
    /// range, so this is what a piped `tt-devpro settle` does. The per-day prompt
    /// has the same problem as the batch one, and the branch answers it by fetching
    /// every unfilled day up front and rendering the summary.
    #[tokio::test]
    async fn a_day_by_day_run_with_no_console_prints_the_summary_instead_of_prompting() {
        let chrono = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&chrono_body(&[work_entry(
                1,
                "2026-09-18",
                "Alpha - DevPro - Work",
                "Friday work",
            )])),
            crate::api::stub::json_200(&chrono_body(&[work_entry(
                2,
                "2026-09-18",
                "Alpha - DevPro - Work",
                "Friday work",
            )])),
        ]);
        let portal = crate::api::stub::StubServer::start(vec![
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&normal_view_body(&[])),
            crate::api::stub::json_200(&user_body("u-1")),
            crate::api::stub::json_200(&projects_body("u-1", &[("id-alpha", "Alpha")])),
        ]);
        let config = settle_config(&chrono.base_url, &[("Alpha - DevPro - Work", "Alpha")]);
        let mut io = FakeConsole::new().absent();

        run_settle(
            &SettleArgs::default(),
            &config,
            &portal.base_url,
            d("2026-09-20"),
            &mut io,
        )
        .await
        .expect("the scan completes");

        assert!(io.out_text().contains("Friday work"), "{}", io.out_text());
        assert!(
            !io.out_text().contains("days to settle:"),
            "the interactive listing belongs to the branch that prompts: {}",
            io.out_text()
        );
        assert!(
            !io.out_text().contains("Cancelled."),
            "an unasked question is not a cancellation: {}",
            io.out_text()
        );
        let requests = portal.requests();
        assert!(
            requests.iter().all(|r| r.method == "GET"),
            "{:?}",
            requests
                .iter()
                .map(|r| (&r.method, &r.target))
                .collect::<Vec<_>>()
        );
        let _ = chrono.requests();
    }
}
