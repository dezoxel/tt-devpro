//! `commands/SettleRenderer.kt`, ported.
//!
//! Pure rendering helpers for settle output: the `--dry-run` summary block, the
//! two per-row helpers the interactive draft table uses, and — new here — the
//! single copy of C12's task-title derivation that `settle.rs` and `borrower.rs`
//! both call. Nothing does I/O; callers decide how to emit the returned strings.
//!
//! Four JVM behaviours this file reproduces rather than approximates, each one
//! measured against JDK 21 (`21.0.11-graal`, the toolchain the incumbent was
//! built with) rather than reasoned about:
//!
//! 1. **Every printed number goes through [`crate::fmt`].** `SettleRenderer.kt:59,65,73,79`
//!    are `String.format("%.2f")` sites. The values reaching them are quantized
//!    to 0.25 on the live path and therefore safe either way, but the function is
//!    public and its arguments are not validated, so the Java rule is applied
//!    unconditionally. See C25.
//! 2. **Padding counts UTF-16 code units, not `char`s.** `%-${projW}s` pads to
//!    `String.length()`, and `projW` itself comes from `maxOf { …length }`. Rust's
//!    `{:<w$}` counts Unicode scalar values, so the two disagree on anything above
//!    U+FFFF — `"A\u{1F600}B"` is 4 units to Java and 3 chars to Rust.
//! 3. **String ordering is by UTF-16 code unit.** `sortedBy { devproProjectName }`
//!    goes through `String.compareTo`, which compares code units; Rust's `str`
//!    ordering compares UTF-8 bytes, i.e. code points. Measured divergence:
//!    Java puts U+1F600 before U+E000, Rust puts U+E000 first.
//! 4. **`$` in a Java regex also matches before a single final line terminator,
//!    without consuming it.** The date-suffix strip therefore leaves a trailing
//!    newline in place while still removing the date in front of it. See
//!    [`strip_trailing_date`].
//!
//! `groupBy(…).toSortedMap()` at `SettleRenderer.kt:18,51` is the one place in
//! the codebase where key-sorted order is what the incumbent wants, so it maps to
//! `BTreeMap` — unlike the five bare `groupBy` sites, which need encounter order.
//! Values inside each group keep input order, which matters because `sumOf` is a
//! left fold over `f64` and is therefore order-sensitive. See C28.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate, Weekday};

use crate::fmt::{java_fmt, java_fmt_width};
use crate::model::{ActionType, SettleAction};

/// `"Development work"`, the C12 fallback when an aggregate carries no
/// descriptions. `SettleCommand.kt:495`.
const NO_DESCRIPTION_TITLE: &str = "Development work";

/// The three-letter month names of the date-suffix pattern, in the order the
/// Kotlin alternation lists them. `SettleCommand.kt:484`, `BorrowerService.kt:145`.
const MONTH_ABBREVIATIONS: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

/// Minimum width of the project column. `SettleRenderer.kt:52`.
const MIN_PROJECT_WIDTH: usize = 12;

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// `SettleRenderer.kt:17-22`.
///
/// Days whose proposed hours fall short of 8h, paired with their total, sorted
/// by date. The comparison is `total < 8.0 - 0.01`, an epsilon against float
/// noise — so a day on exactly `7.99` is *not* flagged.
pub fn under_eight_days(actions: &[SettleAction]) -> Vec<(NaiveDate, f64)> {
    group_by_date(actions)
        .into_iter()
        .filter_map(|(date, day)| {
            let total = sum_hours(day.iter().copied());
            if total < 8.0 - 0.01 {
                Some((date, total))
            } else {
                None
            }
        })
        .collect()
}

/// `SettleRenderer.kt:25-26`. Entry type label, independent of filler/borrowed
/// origin — a borrowed *meeting* still reads `Meeting`.
pub fn entry_type(action: &SettleAction) -> &'static str {
    if action.is_meeting { "Meeting" } else { "Work" }
}

/// `SettleRenderer.kt:32-41`.
///
/// Chrono entry text for the interactive draft table: a marker for synthetic
/// entries, otherwise every description with its trailing `" - <chronoProject>"`
/// removed, joined by `"; "`.
///
/// Note this is *not* the full C12 derivation — it strips the project suffix but
/// **not** the trailing date, where [`task_title`] strips both. The oracle's C12
/// text says the same cleaning is applied here; the source says otherwise, and
/// the source is what ships.
pub fn clean_chrono_entry(action: &SettleAction) -> String {
    if action.is_filler {
        return "[filler]".to_string();
    }
    if action.is_borrowed {
        return "[borrowed]".to_string();
    }
    let project_suffix = format!(" - {}", action.aggregate.chrono_project);
    action
        .aggregate
        .descriptions
        .iter()
        .map(|desc| desc.strip_suffix(&project_suffix).unwrap_or(desc))
        .collect::<Vec<_>>()
        .join("; ")
}

/// C12, the derivation itself. `SettleCommand.kt:489-491` and the textually
/// identical `BorrowerService.kt:146-148`, collapsed into one function both sides
/// call.
///
/// Strip a trailing `" - <chronoProject>"`, then strip a trailing `, Mon D YYYY`.
/// Order matters: the borrower's own comment says "remove project suffix and date
/// pattern", and a description carrying both has the suffix outermost.
pub fn clean_task_title(description: &str, chrono_project: &str) -> String {
    let project_suffix = format!(" - {chrono_project}");
    let without_suffix = description
        .strip_suffix(&project_suffix)
        .unwrap_or(description);
    strip_trailing_date(without_suffix)
}

/// C12 at its primary site, `SettleCommand.kt:487-496`: the first description,
/// cleaned by [`clean_task_title`], or `"Development work"` when there are none.
///
/// `BorrowerService` has no empty case — it always holds one description string —
/// which is why the empty-input rule lives in this wrapper and not in
/// [`clean_task_title`].
pub fn task_title(descriptions: &[String], chrono_project: &str) -> String {
    match descriptions.first() {
        Some(first) => clean_task_title(first, chrono_project),
        None => NO_DESCRIPTION_TITLE.to_string(),
    }
}

/// `SettleRenderer.kt:48-83`. The `--dry-run` summary block, whole.
///
/// Days ascending, rows inside a day by project name ascending (stable, so ties
/// keep input order), a per-day header, a grand total, and — only when some day
/// falls short — the under-8h warning list.
pub fn render_day_summary(actions: &[SettleAction]) -> String {
    if actions.is_empty() {
        return "No actions to settle.".to_string();
    }

    let by_date = group_by_date(actions);

    // `maxOf { it.aggregate.devproProjectName.length }.coerceAtLeast(12)`.
    // `.length` is UTF-16 units on both sides of that expression.
    let project_width = actions
        .iter()
        .map(|a| utf16_len(&a.aggregate.devpro_project_name))
        .max()
        .expect("actions is non-empty here")
        .max(MIN_PROJECT_WIDTH);

    let mut out = String::new();
    for (date, day_actions) in &by_date {
        let weekday = weekday_abbreviation(*date);
        let day_total = sum_hours(day_actions.iter().copied());
        let entry_word = if day_actions.len() == 1 {
            "entry"
        } else {
            "entries"
        };
        // `"%s %s \u{2014} %d %s \u{2192} %.2fh\n"`, SettleRenderer.kt:59.
        out.push_str(&format!(
            "{} {} \u{2014} {} {} \u{2192} {}h\n",
            date,
            weekday,
            day_actions.len(),
            entry_word,
            java_fmt(day_total, 2),
        ));

        let mut sorted: Vec<&SettleAction> = day_actions.clone();
        sorted.sort_by(|l, r| {
            utf16_cmp(
                &l.aggregate.devpro_project_name,
                &r.aggregate.devpro_project_name,
            )
        });
        for a in sorted {
            // `"  %-${projW}s  %-7s  %5.2f  %-6s  \"%s\"\n"`, SettleRenderer.kt:65-66.
            out.push_str(&format!(
                "  {}  {}  {}  {}  \"{}\"\n",
                pad_right(&a.aggregate.devpro_project_name, project_width),
                pad_right(entry_type(a), 7),
                java_fmt_width(a.normalized_hours, 2, 5),
                pad_right(&action_label(a.action), 6),
                a.task_title,
            ));
        }
        out.push('\n');
    }

    // Summed over the whole input in input order, not per day — a different fold
    // order from the per-day headers, and therefore not necessarily their sum.
    let grand_total = sum_hours(actions.iter());
    let day_word = if by_date.len() == 1 { "day" } else { "days" };
    out.push_str(&format!(
        "Total: {}h across {} {}, {} entries",
        java_fmt(grand_total, 2),
        by_date.len(),
        day_word,
        actions.len(),
    ));

    let under = under_eight_days(actions);
    if !under.is_empty() {
        // U+26A0 U+FE0F and *two* spaces. The other warning in this codebase
        // (`SettleCommand.kt:478`) is a bare U+26A0 with one space; the two are
        // different strings and normalising them breaks byte parity.
        out.push_str("\n\n\u{26A0}\u{FE0F}  Under 8h (borrowed+filler cap reached):");
        for (date, total) in under {
            out.push_str(&format!(
                "\n  {}: {}h (need {}h more)",
                date,
                java_fmt(total, 2),
                java_fmt(8.0 - total, 2),
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// `actions.groupBy { it.aggregate.date }.toSortedMap()` — keys ascending,
/// values in input order within each key.
fn group_by_date(actions: &[SettleAction]) -> BTreeMap<NaiveDate, Vec<&SettleAction>> {
    let mut by_date: BTreeMap<NaiveDate, Vec<&SettleAction>> = BTreeMap::new();
    for action in actions {
        by_date
            .entry(action.aggregate.date)
            .or_default()
            .push(action);
    }
    by_date
}

/// Kotlin's `sumOf { it.normalizedHours }` — a left fold from `0.0`, which for
/// `f64` is not the same as any other association order.
fn sum_hours<'a>(actions: impl IntoIterator<Item = &'a SettleAction>) -> f64 {
    actions
        .into_iter()
        .fold(0.0, |acc, a| acc + a.normalized_hours)
}

/// `String.length` — UTF-16 code units, which is what `%-Ns` pads to.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `java.lang.String.compareTo` — lexicographic over UTF-16 code units.
fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `String.format("%-<width>s", s)`. Left-justified, space-padded to `width`
/// UTF-16 units, never truncated.
fn pad_right(s: &str, width: usize) -> String {
    let len = utf16_len(s);
    if len >= width {
        s.to_string()
    } else {
        let mut padded = String::with_capacity(s.len() + (width - len));
        padded.push_str(s);
        for _ in 0..(width - len) {
            padded.push(' ');
        }
        padded
    }
}

/// Kotlin's `name.lowercase().replaceFirstChar { it.uppercase() }`: `CREATE` →
/// `Create`, `MONDAY` → `Monday`.
fn title_case(name: &str) -> String {
    let lowered = name.to_lowercase();
    let mut chars = lowered.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `date.dayOfWeek.name.lowercase().replaceFirstChar { it.uppercase() }.take(3)`,
/// `SettleRenderer.kt:56`. `java.time.DayOfWeek` names are the source strings.
fn weekday_abbreviation(date: NaiveDate) -> String {
    let name = match date.weekday() {
        Weekday::Mon => "MONDAY",
        Weekday::Tue => "TUESDAY",
        Weekday::Wed => "WEDNESDAY",
        Weekday::Thu => "THURSDAY",
        Weekday::Fri => "FRIDAY",
        Weekday::Sat => "SATURDAY",
        Weekday::Sun => "SUNDAY",
    };
    title_case(name).chars().take(3).collect()
}

/// `a.action.name.lowercase().replaceFirstChar { it.uppercase() }`,
/// `SettleRenderer.kt:64`. kotlinx enum names are the declared ones.
fn action_label(action: ActionType) -> String {
    let name = match action {
        ActionType::Create => "CREATE",
        ActionType::Update => "UPDATE",
        ActionType::Skip => "SKIP",
    };
    title_case(name)
}

/// `Regex(", (Jan|…|Dec) \d{1,2} \d{4}$").replace(s, "")`, reproduced.
///
/// Three things a `regex`-crate translation gets wrong, all measured on JDK 21:
///
/// - Java's `$` without `MULTILINE` matches at end of input **and** just before a
///   single final line terminator (`\n`, `\r\n`, `\r`, U+0085, U+2028, U+2029),
///   and it does not consume it. `"Event, Sep 18 2026\n"` becomes `"Event\n"`, not
///   `"Event"`. Two terminators in a row kill the match: `"…2026\n\n"` is left
///   alone, because the date is no longer adjacent to the final terminator.
/// - `\d` is ASCII-only by default: Arabic-Indic digits do not match.
/// - `\d{1,2}` is greedy with backtracking, so the leftmost match wins — two
///   digits are tried before one.
///
/// The crate is not a dependency of this project anyway, so this is a hand port
/// rather than a choice between the two.
fn strip_trailing_date(s: &str) -> String {
    let (body, terminator) = split_final_line_terminator(s);
    match date_suffix_start(body) {
        Some(start) => {
            let mut out = String::with_capacity(start + terminator.len());
            out.push_str(&body[..start]);
            out.push_str(terminator);
            out
        }
        None => s.to_string(),
    }
}

/// Split off a single trailing line terminator, the one Java's `$` is allowed to
/// match in front of. `\r\n` counts as one.
fn split_final_line_terminator(s: &str) -> (&str, &str) {
    if let Some(body) = s.strip_suffix("\r\n") {
        return (body, "\r\n");
    }
    for terminator in ["\n", "\r", "\u{85}", "\u{2028}", "\u{2029}"] {
        if let Some(body) = s.strip_suffix(terminator) {
            return (body, terminator);
        }
    }
    (s, "")
}

/// Byte index at which a `, Mon D YYYY` suffix starts, or `None`.
///
/// Matched right to left, which is equivalent to Java's leftmost-match scan here
/// because everything to the right of the day is fixed width: the only ambiguity
/// is the day's one-or-two digits, and trying two first reproduces both the
/// greedy quantifier and the leftmost start position.
fn date_suffix_start(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();

    // `\d{4}` at the very end.
    let mut cursor = bytes.len().checked_sub(4)?;
    if !bytes[cursor..].iter().all(u8::is_ascii_digit) {
        return None;
    }

    // ` ` before the year.
    cursor = cursor.checked_sub(1)?;
    if bytes[cursor] != b' ' {
        return None;
    }

    for day_digits in [2usize, 1] {
        let Some(day_start) = cursor.checked_sub(day_digits) else {
            continue;
        };
        if !bytes[day_start..cursor].iter().all(u8::is_ascii_digit) {
            continue;
        }

        // ` ` before the day.
        let Some(space) = day_start.checked_sub(1) else {
            continue;
        };
        if bytes[space] != b' ' {
            continue;
        }

        // The three-letter month.
        let Some(month_start) = space.checked_sub(3) else {
            continue;
        };
        if !MONTH_ABBREVIATIONS.contains(&&bytes[month_start..space]) {
            continue;
        }

        // `, ` before the month.
        let Some(comma) = month_start.checked_sub(2) else {
            continue;
        };
        if &bytes[comma..month_start] != b", " {
            continue;
        }

        return Some(comma);
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// The Kotlin test's `action(...)` helper (`DaySummaryTest.kt:16-43`), which
    /// leans on default arguments Rust does not have.
    struct Row {
        date: &'static str,
        devpro_project: String,
        hours: f64,
        title: String,
        is_meeting: bool,
        is_filler: bool,
        is_borrowed: bool,
        chrono_project: String,
        descriptions: Option<Vec<String>>,
        action: ActionType,
    }

    impl Row {
        fn new(date: &'static str, devpro_project: &str, hours: f64, title: &str) -> Self {
            Row {
                date,
                devpro_project: devpro_project.to_string(),
                hours,
                title: title.to_string(),
                is_meeting: false,
                is_filler: false,
                is_borrowed: false,
                chrono_project: "Some Chrono Project".to_string(),
                descriptions: None,
                action: ActionType::Create,
            }
        }

        fn meeting(mut self) -> Self {
            self.is_meeting = true;
            self
        }

        fn filler(mut self) -> Self {
            self.is_filler = true;
            self
        }

        fn borrowed(mut self) -> Self {
            self.is_borrowed = true;
            self
        }

        fn chrono_project(mut self, project: &str) -> Self {
            self.chrono_project = project.to_string();
            self
        }

        fn descriptions(mut self, descriptions: &[&str]) -> Self {
            self.descriptions = Some(descriptions.iter().map(|d| d.to_string()).collect());
            self
        }

        fn action(mut self, action: ActionType) -> Self {
            self.action = action;
            self
        }

        fn build(self) -> SettleAction {
            let descriptions = self
                .descriptions
                .unwrap_or_else(|| vec![self.title.clone()]);
            SettleAction {
                aggregate: crate::model::DayProjectAggregate {
                    date: NaiveDate::parse_from_str(self.date, "%Y-%m-%d").expect("test date"),
                    chrono_project: self.chrono_project,
                    total_hours: self.hours,
                    descriptions,
                    devpro_project_name: self.devpro_project.clone(),
                    billability: "Billable".to_string(),
                    max_hours: None,
                },
                normalized_hours: self.hours,
                is_meeting: self.is_meeting,
                is_filler: self.is_filler,
                is_borrowed: self.is_borrowed,
                source_date: None,
                task_title: self.title,
                devpro_project_id: format!("id-{}", self.devpro_project),
                action: self.action,
                existing_worklog_id: None,
                is_manually_fixed: false,
            }
        }
    }

    fn action(date: &'static str, devpro_project: &str, hours: f64, title: &str) -> SettleAction {
        Row::new(date, devpro_project, hours, title).build()
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("test date")
    }

    // -----------------------------------------------------------------------
    // The eight tests carried over from DaySummaryTest.kt
    // -----------------------------------------------------------------------

    /// `DaySummaryTest.kt:46` — "empty input yields a friendly message".
    /// `SettleRenderer.kt:49`.
    #[test]
    fn empty_input_yields_a_friendly_message() {
        assert_eq!("No actions to settle.", render_day_summary(&[]));
    }

    /// `DaySummaryTest.kt:51` — "groups by day with per-day totals and weekday".
    /// `SettleRenderer.kt:51-59`.
    #[test]
    fn groups_by_day_with_per_day_totals_and_weekday() {
        let out = render_day_summary(&[
            action("2026-07-10", "Velocitor: NLP", 6.0, "Sprint work"),
            Row::new("2026-07-10", "AI Practice", 2.0, "AI Team Sync")
                .meeting()
                .build(),
            action("2026-07-09", "Presales", 8.0, "Discovery"),
        ]);
        // 2026-07-10 is a Friday, 2026-07-09 a Thursday.
        assert!(
            out.contains("2026-07-10 Fri \u{2014} 2 entries \u{2192} 8.00h"),
            "{out}"
        );
        assert!(
            out.contains("2026-07-09 Thu \u{2014} 1 entry \u{2192} 8.00h"),
            "{out}"
        );
        // Days ascending: the Thursday block precedes the Friday one.
        assert!(
            out.find("2026-07-09").expect("thu") < out.find("2026-07-10").expect("fri"),
            "{out}"
        );
    }

    /// `DaySummaryTest.kt:67` — "renders type, hours, action and quoted title per
    /// row". `SettleRenderer.kt:64-66`.
    #[test]
    fn renders_type_hours_action_and_quoted_title_per_row() {
        let out = render_day_summary(&[action("2026-07-10", "Velocitor: NLP", 6.0, "Sprint work")]);
        assert!(out.contains("Velocitor: NLP"), "{out}");
        assert!(out.contains("Work"), "{out}");
        assert!(out.contains("6.00"), "{out}");
        assert!(out.contains("Create"), "{out}");
        assert!(out.contains("\"Sprint work\""), "{out}");
    }

    /// `DaySummaryTest.kt:77` — "grand total sums hours across all days and
    /// entries". `SettleRenderer.kt:71-73`.
    #[test]
    fn grand_total_sums_hours_across_all_days_and_entries() {
        let out = render_day_summary(&[
            action("2026-07-10", "A", 6.0, "x"),
            action("2026-07-10", "B", 2.0, "y"),
            action("2026-07-09", "A", 8.0, "z"),
        ]);
        assert!(
            out.contains("Total: 16.00h across 2 days, 3 entries"),
            "{out}"
        );
    }

    /// `DaySummaryTest.kt:89` — "flags days that fall short of 8h".
    /// `SettleRenderer.kt:75-81`.
    #[test]
    fn flags_days_that_fall_short_of_8h() {
        let out = render_day_summary(&[
            action("2026-07-03", "A", 4.5, "x"), // under 8h
            action("2026-07-02", "B", 8.0, "y"), // exactly 8h — not flagged
        ]);
        assert!(out.contains("\u{26A0}\u{FE0F}  Under 8h"), "{out}");
        assert!(out.contains("2026-07-03: 4.50h (need 3.50h more)"), "{out}");
        assert!(!out.contains("2026-07-02:"), "{out}");
    }

    /// `DaySummaryTest.kt:102` — "no warning when every day reaches 8h".
    /// `SettleRenderer.kt:76`.
    #[test]
    fn no_warning_when_every_day_reaches_8h() {
        let out = render_day_summary(&[action("2026-07-02", "A", 8.0, "y")]);
        assert!(!out.contains("Under 8h"), "{out}");
    }

    /// `DaySummaryTest.kt:108` — "entryType reflects meeting flag".
    /// `SettleRenderer.kt:25-26`.
    #[test]
    fn entry_type_reflects_meeting_flag() {
        assert_eq!(
            "Meeting",
            entry_type(&Row::new("2026-07-10", "A", 1.0, "sync").meeting().build())
        );
        assert_eq!("Work", entry_type(&action("2026-07-10", "A", 1.0, "code")));
    }

    /// `DaySummaryTest.kt:114` — "cleanChronoEntry strips project suffix and marks
    /// synthetic entries". `SettleRenderer.kt:32-41`.
    #[test]
    fn clean_chrono_entry_strips_project_suffix_and_marks_synthetic_entries() {
        let normal = Row::new("2026-07-10", "A", 1.0, "Wrote docs")
            .chrono_project("MyProj")
            .descriptions(&["Wrote docs - MyProj"])
            .build();
        assert_eq!("Wrote docs", clean_chrono_entry(&normal));
        assert_eq!(
            "[filler]",
            clean_chrono_entry(&Row::new("2026-07-10", "A", 1.0, "f").filler().build())
        );
        assert_eq!(
            "[borrowed]",
            clean_chrono_entry(&Row::new("2026-07-10", "A", 1.0, "b").borrowed().build())
        );
    }

    // -----------------------------------------------------------------------
    // The captured incumbent output, byte for byte
    // -----------------------------------------------------------------------

    /// The strongest test in this file: the incumbent's own `settle --json` dump
    /// of the 2026-09-18 run, fed back through this renderer, must reproduce the
    /// incumbent's `settle --dry-run` stdout byte for byte. Both files were
    /// captured from the same binary minutes apart, and the dry-run capture is
    /// verified byte-identical across two consecutive runs, so it is a real
    /// oracle rather than a snapshot of this implementation.
    ///
    /// `~/.cache/tt-devpro-rewrite/baseline/settle-json.out` →
    /// `~/.cache/tt-devpro-rewrite/baseline/settle-dryrun.out`.
    #[test]
    fn the_captured_2026_09_18_dry_run_is_reproduced_byte_for_byte() {
        let actions: Vec<SettleAction> =
            serde_json::from_str(baseline::SETTLE_JSON).expect("captured --json parses");
        assert_eq!(9, actions.len(), "the captured run had nine actions");

        // The capture is a whole stdout stream; the renderer returns the block
        // without the trailing newline `echo` adds.
        let expected = baseline::SETTLE_DRYRUN
            .strip_suffix('\n')
            .expect("capture ends in a newline");
        assert_eq!(expected, render_day_summary(&actions));
    }

    /// The warning glyph in the captured output, asserted as bytes rather than as
    /// a literal an editor could silently normalise: U+26A0 U+FE0F followed by
    /// **two** spaces. `SettleCommand.kt:478` carries a bare U+26A0 with **one**
    /// space for a different warning; collapsing the two breaks this.
    #[test]
    fn the_under_8h_warning_carries_the_variation_selector_and_two_spaces() {
        let out = render_day_summary(&[action("2026-07-03", "A", 4.5, "x")]);
        let marker = "\u{26A0}\u{FE0F}  Under 8h";
        assert_eq!(
            &[0xE2, 0x9A, 0xA0, 0xEF, 0xB8, 0x8F, 0x20, 0x20][..],
            &marker.as_bytes()[..8],
            "the marker's own bytes"
        );
        assert!(out.contains(marker), "{out}");
        // The bare-glyph form, which is the other warning in the codebase.
        assert!(!out.contains("\u{26A0} "), "{out}");
        assert!(!out.contains("\u{26A0}  Under"), "{out}");
    }

    /// The same renderer over eleven days, all of them full, from the captured
    /// `--from/--to` run. Proves the plural day word, the widest-project column
    /// width (18, `"Delivery Practices"`), the singular `1 entry` day and the
    /// absence of a warning block on a fully-settled range.
    ///
    /// Weaker than the test above: the input here is parsed back out of the
    /// expected output, so grouping and row order are inherited rather than
    /// checked. Widths, number formatting, totals, plurals and the blank-line
    /// layout are checked for real.
    #[test]
    fn the_captured_september_range_dry_run_is_reproduced_byte_for_byte() {
        let expected = baseline::SETTLE_RANGE_DRYRUN
            .strip_suffix('\n')
            .expect("capture ends in a newline");
        let actions = parse_captured_summary(expected);
        assert_eq!(63, actions.len(), "the captured range had 63 entries");
        assert_eq!(expected, render_day_summary(&actions));
    }

    /// Reads a captured summary block back into the actions that produced it.
    /// Test-only, and deliberately strict: anything it cannot parse panics rather
    /// than being skipped, so a malformed fixture cannot quietly shrink the test.
    fn parse_captured_summary(text: &str) -> Vec<SettleAction> {
        let mut actions = Vec::new();
        let mut current_date: Option<NaiveDate> = None;
        for line in text.lines() {
            if line.is_empty() || line.starts_with("Total: ") {
                continue;
            }
            if !line.starts_with("  ") {
                let day = line.split(' ').next().expect("header starts with a date");
                current_date = Some(date(day));
                continue;
            }
            let row = line.trim_start();
            let quote = row.find('"').expect("row carries a quoted title");
            let title = row[quote + 1..]
                .strip_suffix('"')
                .expect("row ends with a quote");
            // `[…project words, type, hours, action]` — the project name is the
            // only field that can contain spaces, so it is whatever is left.
            let head: Vec<&str> = row[..quote].split_whitespace().collect();
            let split = head.len() - 3;
            let project = head[..split].join(" ");
            let row_type = head[split];
            let hours: f64 = head[split + 1].parse().expect("hours parse");
            let mut built = Row::new("2026-01-01", &project, hours, title)
                .action(match head[split + 2] {
                    "Create" => ActionType::Create,
                    "Update" => ActionType::Update,
                    "Skip" => ActionType::Skip,
                    other => panic!("unknown action {other}"),
                })
                .build();
            built.is_meeting = row_type == "Meeting";
            built.aggregate.date = current_date.expect("a row before any header");
            actions.push(built);
        }
        actions
    }

    // -----------------------------------------------------------------------
    // The under-8h boundary
    // -----------------------------------------------------------------------

    /// `SettleRenderer.kt:21` is `total < 8.0 - 0.01`, so `7.99` is the first
    /// value that is *not* flagged. A port written as `total < 7.99` by eye would
    /// pass this; a port written as `total < 8.0` would not.
    #[test]
    fn a_day_on_exactly_7_99_is_not_flagged() {
        assert_eq!(
            Vec::<(NaiveDate, f64)>::new(),
            under_eight_days(&[action("2026-07-03", "A", 7.99, "x")])
        );
    }

    /// The other side of the same epsilon.
    #[test]
    fn a_day_just_below_the_epsilon_is_flagged() {
        assert_eq!(
            vec![(date("2026-07-03"), 7.98)],
            under_eight_days(&[action("2026-07-03", "A", 7.98, "x")])
        );
    }

    /// `SettleRenderer.kt:21` compares against the subtraction `8.0 - 0.01`, and
    /// the obvious worry is that it differs from the literal `7.99`. Measured on
    /// rustc 1.91.1: it does **not** — both are bit pattern `401ff5c28f5c28f6`.
    /// Recorded so nobody re-derives it, and so the boundary below is understood
    /// as an exclusive `<` rather than as float noise.
    #[test]
    fn the_epsilon_subtraction_lands_exactly_on_the_f64_nearest_7_99() {
        assert_eq!((8.0_f64 - 0.01).to_bits(), 7.99_f64.to_bits());
        // The threshold is exclusive, exercised through the function rather than
        // as a constant comparison the compiler folds away.
        assert!(under_eight_days(&[action("2026-07-03", "A", 7.99, "x")]).is_empty());
        assert_eq!(
            vec![(date("2026-07-03"), 7.9899999)],
            under_eight_days(&[action("2026-07-03", "A", 7.9899999, "x")])
        );
    }

    /// A day on exactly 8h is the ordinary settled case and must not be flagged.
    #[test]
    fn a_day_on_exactly_8h_is_not_flagged() {
        let out = render_day_summary(&[action("2026-07-02", "A", 8.0, "y")]);
        assert!(!out.contains("Under 8h"), "{out}");
        assert!(under_eight_days(&[action("2026-07-02", "A", 8.0, "y")]).is_empty());
    }

    /// Over 8h is not flagged either — the check is one-sided.
    #[test]
    fn a_day_over_8h_is_not_flagged() {
        assert!(under_eight_days(&[action("2026-07-02", "A", 8.01, "y")]).is_empty());
    }

    /// The day total is the sum of its rows, not any single row.
    #[test]
    fn the_under_8h_total_is_the_sum_of_the_days_rows() {
        let under = under_eight_days(&[
            action("2026-07-03", "A", 4.0, "x"),
            action("2026-07-03", "B", 3.5, "y"),
        ]);
        assert_eq!(vec![(date("2026-07-03"), 7.5)], under);
    }

    /// `toSortedMap()` at `SettleRenderer.kt:18`: the flagged days come back by
    /// date ascending whatever order they arrived in.
    #[test]
    fn flagged_days_come_back_sorted_by_date_regardless_of_input_order() {
        let under = under_eight_days(&[
            action("2026-07-09", "A", 1.0, "c"),
            action("2026-07-03", "A", 2.0, "a"),
            action("2026-07-07", "A", 3.0, "b"),
        ]);
        assert_eq!(
            vec![
                (date("2026-07-03"), 2.0),
                (date("2026-07-07"), 3.0),
                (date("2026-07-09"), 1.0),
            ],
            under
        );
    }

    /// An empty input has no days to fall short of anything.
    #[test]
    fn under_eight_days_of_nothing_is_empty() {
        assert!(under_eight_days(&[]).is_empty());
    }

    /// The shortfall printed after `need` is `8.0 - total`, computed fresh rather
    /// than accumulated. `SettleRenderer.kt:79`.
    #[test]
    fn the_shortfall_is_eight_minus_the_day_total() {
        let out = render_day_summary(&[action("2026-07-03", "A", 0.25, "x")]);
        assert!(out.contains("2026-07-03: 0.25h (need 7.75h more)"), "{out}");
    }

    /// Several short days each get their own line, in date order.
    #[test]
    fn every_short_day_gets_its_own_warning_line_in_date_order() {
        let out = render_day_summary(&[
            action("2026-07-09", "A", 1.0, "c"),
            action("2026-07-03", "A", 2.0, "a"),
        ]);
        let expected = "\u{26A0}\u{FE0F}  Under 8h (borrowed+filler cap reached):\n  \
             2026-07-03: 2.00h (need 6.00h more)\n  2026-07-09: 1.00h (need 7.00h more)";
        assert!(out.ends_with(expected), "{out}");
    }

    // -----------------------------------------------------------------------
    // Number formatting — C25
    // -----------------------------------------------------------------------

    /// C25. `0.125` is the canonical divergence: Java's `%.2f` rounds the shortest
    /// decimal representation HALF_UP and prints `0.13`; Rust's `{:.2}` rounds the
    /// exact binary value half-to-even and prints `0.12`. A port that used `{:.2}`
    /// would produce the second string here.
    #[test]
    fn row_hours_use_javas_half_up_rule_not_rusts() {
        let out = render_day_summary(&[action("2026-07-03", "A", 0.125, "x")]);
        assert!(out.contains(" 0.13  Create"), "{out}");
        assert!(!out.contains(" 0.12  Create"), "{out}");
        // The naive port, spelled out so this test cannot pass by coincidence.
        assert_eq!("0.12", format!("{:.2}", 0.125_f64));
    }

    /// The same rule on the day header and the grand total, which are separate
    /// format sites (`SettleRenderer.kt:59` and `:73`).
    #[test]
    fn day_header_and_grand_total_use_the_same_half_up_rule() {
        let out = render_day_summary(&[action("2026-07-03", "A", 0.125, "x")]);
        assert!(
            out.starts_with("2026-07-03 Fri \u{2014} 1 entry \u{2192} 0.13h\n"),
            "{out}"
        );
        assert!(
            out.contains("Total: 0.13h across 1 day, 1 entries"),
            "{out}"
        );
    }

    /// `%5.2f` right-justifies in a five-character field, so a sub-10h figure
    /// carries one leading space and a three-digit one is not truncated.
    #[test]
    fn hours_are_right_justified_in_a_five_wide_field() {
        let out = render_day_summary(&[action("2026-07-03", "A", 0.5, "x")]);
        assert!(out.contains("  Work      0.50  Create"), "{out}");

        let wide = render_day_summary(&[action("2026-07-03", "A", 123.456, "x")]);
        assert!(wide.contains("  Work     123.46  Create"), "{wide}");
    }

    /// A zero-duration row still renders, with the same field widths.
    #[test]
    fn a_zero_hour_row_renders_as_0_00() {
        let out = render_day_summary(&[action("2026-07-03", "A", 0.0, "x")]);
        assert!(out.contains("  Work      0.00  Create"), "{out}");
        assert!(out.contains("2026-07-03: 0.00h (need 8.00h more)"), "{out}");
    }

    // -----------------------------------------------------------------------
    // Column widths — UTF-16, not chars
    // -----------------------------------------------------------------------

    /// `SettleRenderer.kt:52`: the project column is at least 12 wide even when
    /// every name is shorter.
    #[test]
    fn the_project_column_is_never_narrower_than_twelve() {
        let out = render_day_summary(&[action("2026-07-03", "A", 1.0, "x")]);
        assert!(out.contains("  A             Work "), "{out}");
    }

    /// …and widens to the longest name when one exceeds the floor, for every row
    /// in the block including the short ones.
    #[test]
    fn the_project_column_widens_to_the_longest_name_across_all_days() {
        let out = render_day_summary(&[
            action("2026-07-03", "Short", 1.0, "x"),
            action("2026-07-04", "A Considerably Longer Project", 1.0, "y"),
        ]);
        assert!(
            out.contains("  Short                          Work "),
            "{out}"
        );
        assert!(
            out.contains("  A Considerably Longer Project  Work "),
            "{out}"
        );
    }

    /// The measured UTF-16 case. `"A\u{1F600}B"` is three `char`s and four UTF-16
    /// code units; Java pads to the latter. Measured on JDK 21:
    /// `String.format("%-6s", "A😀B")` yields the string plus **two**
    /// spaces. Rust's `{:<6}` would add three.
    #[test]
    fn column_width_counts_utf16_units_so_an_emoji_project_name_pads_two_short() {
        assert_eq!(4, utf16_len("A\u{1F600}B"));
        assert_eq!(3, "A\u{1F600}B".chars().count());
        assert_eq!("A\u{1F600}B  ", pad_right("A\u{1F600}B", 6));
        // The naive port, so this test has something to fail against.
        assert_eq!("A\u{1F600}B   ", format!("{:<6}", "A\u{1F600}B"));
    }

    /// A BMP non-ASCII name — where UTF-16 units, `char`s and the visual width
    /// agree but *bytes* do not — still pads by units, not by bytes.
    #[test]
    fn column_width_counts_units_not_bytes_for_cyrillic() {
        assert_eq!(6, utf16_len("Привет"));
        assert_eq!(12, "Привет".len());
        assert_eq!("Привет      ", pad_right("Привет", 12));
    }

    /// Java never truncates on `%-Ns`; a name wider than the field overflows it.
    #[test]
    fn a_name_wider_than_the_field_is_not_truncated() {
        assert_eq!("abcdefgh", pad_right("abcdefgh", 3));
    }

    /// A very long title runs to the end of the line untouched — the summary has
    /// no truncation at all, unlike the interactive draft table.
    #[test]
    fn a_very_long_title_is_never_truncated_or_ellipsised() {
        let title = "Map every Connect issue to its producer and its owner, then write the whole \
                     thing up for the Thursday sync and circulate it before the call";
        let out = render_day_summary(&[action("2026-07-03", "A", 1.0, title)]);
        assert!(out.contains(&format!("\"{title}\"")), "{out}");
        assert!(!out.contains('\u{2026}'), "{out}");
    }

    // -----------------------------------------------------------------------
    // Ordering — C28
    // -----------------------------------------------------------------------

    /// C28's cheap guard: the same input rendered twice must give the same bytes.
    /// Fails instantly if a randomized `HashMap` ever replaces the `BTreeMap`.
    #[test]
    fn rendering_the_same_input_twice_gives_identical_output() {
        let actions: Vec<SettleAction> = (0..12)
            .map(|i| {
                action(
                    if i % 3 == 0 {
                        "2026-07-03"
                    } else if i % 3 == 1 {
                        "2026-07-04"
                    } else {
                        "2026-07-05"
                    },
                    &format!("Project {}", i % 4),
                    0.25,
                    &format!("task {i}"),
                )
            })
            .collect();
        assert_eq!(render_day_summary(&actions), render_day_summary(&actions));
    }

    /// `sortedBy` is TimSort and therefore stable: two rows tying on project name
    /// keep their input order. `sort_unstable_by` compiles just as well and would
    /// be free to swap them.
    #[test]
    fn rows_tying_on_project_name_keep_their_input_order() {
        let out = render_day_summary(&[
            action("2026-07-03", "Same", 1.0, "first"),
            action("2026-07-03", "Same", 1.0, "second"),
            action("2026-07-03", "Same", 1.0, "third"),
        ]);
        let first = out.find("\"first\"").expect("first");
        let second = out.find("\"second\"").expect("second");
        let third = out.find("\"third\"").expect("third");
        assert!(first < second && second < third, "{out}");
    }

    /// Rows inside a day are sorted by project name ascending.
    #[test]
    fn rows_are_sorted_by_project_name_within_a_day() {
        let out = render_day_summary(&[
            action("2026-07-03", "Zeta", 1.0, "z"),
            action("2026-07-03", "Alpha", 1.0, "a"),
            action("2026-07-03", "Mid", 1.0, "m"),
        ]);
        let a = out.find("Alpha").expect("alpha");
        let m = out.find("Mid").expect("mid");
        let z = out.find("Zeta").expect("zeta");
        assert!(a < m && m < z, "{out}");
    }

    /// Sorting is per day, not across the whole block: each day restarts.
    #[test]
    fn sorting_restarts_inside_each_day() {
        let out = render_day_summary(&[
            action("2026-07-03", "Zeta", 1.0, "z1"),
            action("2026-07-03", "Alpha", 1.0, "a1"),
            action("2026-07-04", "Zeta", 1.0, "z2"),
            action("2026-07-04", "Alpha", 1.0, "a2"),
        ]);
        let order: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("  A") || l.starts_with("  Z"))
            .map(|l| l.split_whitespace().next().expect("project"))
            .collect();
        assert_eq!(vec!["Alpha", "Zeta", "Alpha", "Zeta"], order);
    }

    /// `String.compareTo` compares UTF-16 code units, so a supplementary-plane
    /// character sorts *before* U+E000 — the opposite of Rust's byte ordering.
    /// Measured: `"".compareTo("😀")` is `1987` on JDK 21.
    #[test]
    fn project_names_sort_by_utf16_code_unit_not_by_code_point() {
        let emoji = "\u{1F600}";
        let private_use = "\u{E000}";
        assert_eq!(Ordering::Greater, utf16_cmp(private_use, emoji));
        // Rust's native ordering, which is the naive port and disagrees.
        assert_eq!(Ordering::Less, private_use.cmp(emoji));

        let out = render_day_summary(&[
            action("2026-07-03", private_use, 1.0, "pua"),
            action("2026-07-03", emoji, 1.0, "emoji"),
        ]);
        assert!(
            out.find("\"emoji\"").expect("emoji") < out.find("\"pua\"").expect("pua"),
            "{out}"
        );
    }

    /// Days ascending across a month and a year boundary, not lexicographically
    /// by anything else.
    #[test]
    fn days_are_ordered_across_month_and_year_boundaries() {
        let out = render_day_summary(&[
            action("2027-01-01", "A", 8.0, "new year"),
            action("2026-12-31", "A", 8.0, "old year"),
            action("2026-11-30", "A", 8.0, "november"),
        ]);
        let nov = out.find("2026-11-30").expect("nov");
        let dec = out.find("2026-12-31").expect("dec");
        let jan = out.find("2027-01-01").expect("jan");
        assert!(nov < dec && dec < jan, "{out}");
    }

    // -----------------------------------------------------------------------
    // Layout details
    // -----------------------------------------------------------------------

    /// Singular and plural on both counters, which are decided separately
    /// (`SettleRenderer.kt:58` and `:72`).
    #[test]
    fn entry_and_day_words_are_singular_only_at_one() {
        let one = render_day_summary(&[action("2026-07-03", "A", 8.0, "x")]);
        assert!(one.contains("\u{2014} 1 entry \u{2192}"), "{one}");
        assert!(one.contains("across 1 day, 1 entries"), "{one}");

        let two = render_day_summary(&[
            action("2026-07-03", "A", 8.0, "x"),
            action("2026-07-04", "A", 8.0, "y"),
        ]);
        assert!(two.contains("\u{2014} 1 entry \u{2192}"), "{two}");
        assert!(two.contains("across 2 days, 2 entries"), "{two}");
    }

    /// The grand-total entry counter has no singular form: the incumbent prints
    /// `1 entries`. Ported as-is, because parity is the bar.
    #[test]
    fn the_grand_total_says_one_entries_for_a_single_action() {
        let out = render_day_summary(&[action("2026-07-03", "A", 8.0, "x")]);
        assert!(
            out.contains("Total: 8.00h across 1 day, 1 entries"),
            "{out}"
        );
    }

    /// Each day block ends with a blank line, and the total follows it directly —
    /// so a settled block ends without a trailing newline.
    #[test]
    fn each_day_block_is_followed_by_a_blank_line_and_the_block_has_no_trailing_newline() {
        let out = render_day_summary(&[
            action("2026-07-03", "A", 8.0, "x"),
            action("2026-07-04", "A", 8.0, "y"),
        ]);
        assert_eq!(
            "2026-07-03 Fri \u{2014} 1 entry \u{2192} 8.00h\n  \
             A             Work      8.00  Create  \"x\"\n\n\
             2026-07-04 Sat \u{2014} 1 entry \u{2192} 8.00h\n  \
             A             Work      8.00  Create  \"y\"\n\n\
             Total: 16.00h across 2 days, 2 entries",
            out
        );
    }

    /// Every weekday abbreviation, so a wrong day-of-week mapping cannot hide in
    /// the one date a fixture happens to use.
    #[test]
    fn every_weekday_abbreviation_matches_java_dayofweek() {
        let expected = [
            ("2026-07-06", "Mon"),
            ("2026-07-07", "Tue"),
            ("2026-07-08", "Wed"),
            ("2026-07-09", "Thu"),
            ("2026-07-10", "Fri"),
            ("2026-07-11", "Sat"),
            ("2026-07-12", "Sun"),
        ];
        for (day, abbreviation) in expected {
            assert_eq!(abbreviation, weekday_abbreviation(date(day)), "{day}");
        }
    }

    /// All three action labels, including `SKIP`, which the captured runs never
    /// contain.
    #[test]
    fn every_action_label_is_title_cased() {
        assert_eq!("Create", action_label(ActionType::Create));
        assert_eq!("Update", action_label(ActionType::Update));
        assert_eq!("Skip", action_label(ActionType::Skip));

        let out = render_day_summary(&[Row::new("2026-07-03", "A", 1.0, "x")
            .action(ActionType::Skip)
            .build()]);
        assert!(out.contains("  Skip    \"x\""), "{out}");
    }

    /// A meeting row and a work row differ only in the type column; the type
    /// column is padded to 7 so `Work` carries three trailing spaces.
    #[test]
    fn the_type_column_is_padded_to_seven() {
        let out = render_day_summary(&[
            Row::new("2026-07-03", "A", 1.0, "m").meeting().build(),
            action("2026-07-03", "A", 1.0, "w"),
        ]);
        assert!(out.contains("  Meeting   1.00  "), "{out}");
        assert!(out.contains("  Work      1.00  "), "{out}");
    }

    /// A title carrying a double quote is emitted verbatim — the renderer quotes
    /// but does not escape.
    #[test]
    fn a_title_containing_a_quote_is_not_escaped() {
        let out = render_day_summary(&[action("2026-07-03", "A", 1.0, "the \"big\" sync")]);
        assert!(out.contains("\"the \"big\" sync\""), "{out}");
    }

    /// A day of nothing but meetings renders exactly like any other day — the
    /// meeting flag reaches only the type column.
    #[test]
    fn a_meeting_only_day_still_gets_a_total_and_a_warning() {
        let out = render_day_summary(&[
            Row::new("2026-07-03", "A", 1.0, "sync one")
                .meeting()
                .build(),
            Row::new("2026-07-03", "A", 1.0, "sync two")
                .meeting()
                .build(),
        ]);
        assert!(out.contains("\u{2014} 2 entries \u{2192} 2.00h"), "{out}");
        assert!(out.contains("2026-07-03: 2.00h (need 6.00h more)"), "{out}");
    }

    /// Several days in different states in one block: one full, one short, one
    /// over. Only the short one is listed.
    #[test]
    fn a_block_mixing_full_short_and_over_days_lists_only_the_short_one() {
        let out = render_day_summary(&[
            action("2026-07-03", "A", 8.0, "full"),
            action("2026-07-04", "A", 3.25, "short"),
            action("2026-07-05", "A", 9.5, "over"),
        ]);
        assert!(
            out.contains("Total: 20.75h across 3 days, 3 entries"),
            "{out}"
        );
        assert!(out.contains("2026-07-04: 3.25h (need 4.75h more)"), "{out}");
        assert!(!out.contains("2026-07-03:"), "{out}");
        assert!(!out.contains("2026-07-05:"), "{out}");
    }

    // -----------------------------------------------------------------------
    // cleanChronoEntry
    // -----------------------------------------------------------------------

    /// `SettleRenderer.kt:33-34`: the filler marker wins over the borrowed one
    /// when both flags are set.
    #[test]
    fn filler_wins_over_borrowed_when_both_flags_are_set() {
        let both = Row::new("2026-07-10", "A", 1.0, "x")
            .filler()
            .borrowed()
            .build();
        assert_eq!("[filler]", clean_chrono_entry(&both));
    }

    /// `joinToString("; ")` over several descriptions, each stripped
    /// independently. `SettleRenderer.kt:37-39`.
    #[test]
    fn several_descriptions_are_joined_with_a_semicolon_each_stripped_separately() {
        let action = Row::new("2026-07-10", "A", 1.0, "t")
            .chrono_project("MyProj")
            .descriptions(&["One - MyProj", "Two", "Three - MyProj"])
            .build();
        assert_eq!("One; Two; Three", clean_chrono_entry(&action));
    }

    /// A description that does not end in the suffix is left alone, including one
    /// that merely *contains* it.
    #[test]
    fn a_description_not_ending_in_the_suffix_is_left_alone() {
        let action = Row::new("2026-07-10", "A", 1.0, "t")
            .chrono_project("MyProj")
            .descriptions(&["Wrote docs - MyProj - and more", "MyProj alone"])
            .build();
        assert_eq!(
            "Wrote docs - MyProj - and more; MyProj alone",
            clean_chrono_entry(&action)
        );
    }

    /// A description that is exactly the suffix collapses to an empty string.
    #[test]
    fn a_description_that_is_only_the_suffix_becomes_empty() {
        let action = Row::new("2026-07-10", "A", 1.0, "t")
            .chrono_project("MyProj")
            .descriptions(&[" - MyProj"])
            .build();
        assert_eq!("", clean_chrono_entry(&action));
    }

    /// No descriptions at all gives an empty string — `joinToString` of an empty
    /// list, not a marker.
    #[test]
    fn no_descriptions_give_an_empty_chrono_entry() {
        let action = Row::new("2026-07-10", "A", 1.0, "t")
            .descriptions(&[])
            .build();
        assert_eq!("", clean_chrono_entry(&action));
    }

    /// `cleanChronoEntry` strips the project suffix but **not** the date, unlike
    /// [`task_title`]. The oracle's C12 text says they do the same cleaning; the
    /// source does not, and this test pins the source.
    #[test]
    fn clean_chrono_entry_leaves_a_trailing_date_in_place() {
        let action = Row::new("2026-07-10", "A", 1.0, "t")
            .chrono_project("MyProj")
            .descriptions(&["Standup, Sep 18 2026 - MyProj"])
            .build();
        assert_eq!("Standup, Sep 18 2026", clean_chrono_entry(&action));
        assert_eq!(
            "Standup",
            task_title(&["Standup, Sep 18 2026 - MyProj".to_string()], "MyProj")
        );
    }

    // -----------------------------------------------------------------------
    // C12 — the task-title derivation
    // -----------------------------------------------------------------------

    /// C12's first required case, `SettleCommand.kt:494-495`: no descriptions →
    /// `"Development work"`.
    #[test]
    fn empty_descriptions_yield_the_development_work_title() {
        assert_eq!("Development work", task_title(&[], "Any Project"));
    }

    /// C12's second required case, `SettleCommand.kt:490`: a description ending in
    /// `" - {chronoProject}"` loses the suffix.
    #[test]
    fn a_description_ending_in_the_project_suffix_loses_it() {
        assert_eq!(
            "Wrote docs",
            task_title(
                &["Wrote docs - Practices - DevPro - Work".to_string()],
                "Practices - DevPro - Work"
            )
        );
    }

    /// C12's third required case, `SettleCommand.kt:491`: the old description form
    /// `"Event, Sep 18 2026"` loses its date.
    #[test]
    fn a_description_in_the_old_dated_form_loses_its_date() {
        assert_eq!(
            "AI Heads Sync",
            task_title(&["AI Heads Sync, Sep 18 2026".to_string()], "Some Project")
        );
    }

    /// Both cleanings on one description, in the order the source applies them:
    /// suffix first, then date.
    #[test]
    fn suffix_and_date_are_both_stripped_and_the_suffix_goes_first() {
        assert_eq!(
            "AI Heads Sync",
            task_title(
                &["AI Heads Sync, Sep 18 2026 - Practices - DevPro - Work".to_string()],
                "Practices - DevPro - Work"
            )
        );
        // The other order would leave the date in place, because the date is not
        // at the end until the suffix has gone.
        assert_eq!(
            "AI Heads Sync, Sep 18 2026 - Practices - DevPro - Work",
            strip_trailing_date("AI Heads Sync, Sep 18 2026 - Practices - DevPro - Work")
        );
    }

    /// Only the *first* description is used; the rest are dropped.
    /// `SettleCommand.kt:489`.
    #[test]
    fn only_the_first_description_becomes_the_title() {
        assert_eq!(
            "first",
            task_title(&["first".to_string(), "second".to_string()], "Some Project")
        );
    }

    /// The borrower's entry point takes one description and has no empty case —
    /// an empty string cleans to an empty string rather than to the fallback.
    /// `BorrowerService.kt:146-148`.
    #[test]
    fn the_single_description_entry_point_has_no_development_work_fallback() {
        assert_eq!("", clean_task_title("", "Some Project"));
        assert_eq!("Development work", task_title(&[], "Some Project"));
    }

    /// The suffix is built from the chrono project of the aggregate in hand, so a
    /// mismatched project leaves the description untouched.
    #[test]
    fn a_suffix_from_a_different_project_is_not_stripped() {
        assert_eq!(
            "Wrote docs - OtherProj",
            clean_task_title("Wrote docs - OtherProj", "MyProj")
        );
    }

    /// The measured JDK 21 corpus for the date-suffix regex. Each pair was
    /// produced by running `Pattern.compile(", (Jan|…|Dec) \\d{1,2} \\d{4}$")
    /// .matcher(input).replaceAll("")` on `21.0.11-graal` — the toolchain the
    /// incumbent binary was built with — not by reading the pattern and reasoning.
    #[test]
    fn the_date_suffix_strip_matches_the_measured_jdk21_corpus() {
        let corpus: &[(&str, &str)] = &[
            ("Event, Sep 18 2026", "Event"),
            ("Event, Apr 8 2026", "Event"),
            ("Event, Jan 1 2026", "Event"),
            ("Event, Dec 31 1999", "Event"),
            ("Event, Feb 29 2024", "Event"),
            ("Event", "Event"),
            ("", ""),
            (", Sep 18 2026", ""),
            ("Event, Sep 18 2026\n", "Event\n"),
            ("Event, Sep 18 2026\r\n", "Event\r\n"),
            ("Event, Sep 18 2026\r", "Event\r"),
            ("Event, Sep 18 2026\u{85}", "Event\u{85}"),
            ("Event, Sep 18 2026\u{2028}", "Event\u{2028}"),
            ("Event, Sep 18 2026\u{2029}", "Event\u{2029}"),
            ("Event, Sep 18 2026\n\n", "Event, Sep 18 2026\n\n"),
            ("Event, Sep 18 2026\n\r", "Event, Sep 18 2026\n\r"),
            ("Event, Sep 18 2026 ", "Event, Sep 18 2026 "),
            ("Event, Sep 18 2026 - tail", "Event, Sep 18 2026 - tail"),
            ("A, Sep 18 2026, Oct 1 2026", "A, Sep 18 2026"),
            ("A, Sep 8 18 2026", "A, Sep 8 18 2026"),
            ("A, Sep 118 2026", "A, Sep 118 2026"),
            ("Event, Apr 123 2026", "Event, Apr 123 2026"),
            ("Event, Apr 8 20261", "Event, Apr 8 20261"),
            ("Event, Apr 8 202", "Event, Apr 8 202"),
            ("Event Sep 18 2026", "Event Sep 18 2026"),
            ("Event,Sep 18 2026", "Event,Sep 18 2026"),
            ("Event, sep 18 2026", "Event, sep 18 2026"),
            ("Event, SEP 18 2026", "Event, SEP 18 2026"),
            ("Event, September 18 2026", "Event, September 18 2026"),
            (
                "Event, Sep \u{661}\u{668} 2026",
                "Event, Sep \u{661}\u{668} 2026",
            ),
            (
                "Event, Sep 18 \u{662}\u{660}\u{662}\u{666}",
                "Event, Sep 18 \u{662}\u{660}\u{662}\u{666}",
            ),
            ("Event, Mar 0 2026", "Event"),
            ("Event, Mar 00 0000", "Event"),
            ("Синк, Sep 18 2026", "Синк"),
            ("\u{1F600}, Sep 18 2026", "\u{1F600}"),
            (
                "Event, Sep 18 2026 - Practices - DevPro - Work",
                "Event, Sep 18 2026 - Practices - DevPro - Work",
            ),
            (
                "Sync w Ivan about SWE agents",
                "Sync w Ivan about SWE agents",
            ),
            ("AI Heads Sync, Sep 18 2026", "AI Heads Sync"),
            (", Jan 1 1", ", Jan 1 1"),
            ("x, Jun 30 2026x", "x, Jun 30 2026x"),
            ("Event, Sep 18 2026\t", "Event, Sep 18 2026\t"),
            ("Event, Sep\u{A0}18 2026", "Event, Sep\u{A0}18 2026"),
            ("Event, Sep 18 2026\u{B}", "Event, Sep 18 2026\u{B}"),
            ("Event, Sep 18 2026\u{C}", "Event, Sep 18 2026\u{C}"),
        ];
        for (input, expected) in corpus {
            assert_eq!(*expected, strip_trailing_date(input), "input {input:?}");
        }
    }

    /// The trailing-newline case, stated on its own because it is the one the
    /// plan's `(?:\r?\n)?$` recipe gets wrong: folding the terminator into the
    /// match and replacing with `""` deletes the newline, where Java keeps it.
    /// Measured on JDK 21: `"Event, Sep 18 2026\n"` → `"Event\n"`.
    #[test]
    fn a_trailing_newline_survives_the_date_strip_rather_than_being_eaten() {
        assert_eq!("Event\n", strip_trailing_date("Event, Sep 18 2026\n"));
        assert_ne!("Event", strip_trailing_date("Event, Sep 18 2026\n"));
        assert_eq!(
            "AI Heads Sync\n",
            task_title(
                &["AI Heads Sync, Sep 18 2026\n".to_string()],
                "Some Project"
            )
        );
    }

    /// The other half of the same rule: `$` is allowed in front of **one** final
    /// terminator, so two of them break the match.
    #[test]
    fn two_trailing_newlines_defeat_the_date_strip() {
        assert_eq!(
            "Event, Sep 18 2026\n\n",
            strip_trailing_date("Event, Sep 18 2026\n\n")
        );
    }

    // The four `dateSuffixRegex` cases the team lead re-measured on the JVM,
    // as byte assertions so no escape in a source literal can hide a difference.
    // Each expectation is `Pattern.compile(", (Jan|…|Dec) \d{1,2} \d{4}$")
    // .matcher(s).replaceAll("")` on JDK 21.
    //
    // They also rule out the `regex`-crate recipe that captures the terminator
    // and replaces with `${1}`: that one handles `\n` and `\r\n` but still loses
    // a bare `\r`, U+0085, U+2028 and U+2029, all of which Java's `$` accepts —
    // see the corpus test above. This port has no regex at all.

    /// Case 1 of 4: no terminator, the ordinary path. `"Event, Apr 8 2026"` →
    /// `"Event"`.
    #[test]
    fn jvm_case_1_a_bare_dated_description_loses_exactly_the_date() {
        assert_eq!(
            b"Event".as_slice(),
            strip_trailing_date("Event, Apr 8 2026").as_bytes()
        );
    }

    /// Case 2 of 4: `"Event, Apr 8 2026\n"` → `"Event\n"`. The LF **survives**.
    /// The plan's `(?:\r?\n)?$` recipe returns `"Event"` here.
    #[test]
    fn jvm_case_2_a_trailing_lf_survives_as_its_own_byte() {
        assert_eq!(
            b"Event\n".as_slice(),
            strip_trailing_date("Event, Apr 8 2026\n").as_bytes()
        );
    }

    /// Case 3 of 4: `"Event, Apr 8 2026\r\n"` → `"Event\r\n"`. Java treats CRLF
    /// as one terminator, so **both** bytes survive — a port that only knew about
    /// `\n` would leave a stray `\r` glued to the title.
    #[test]
    fn jvm_case_3_a_trailing_crlf_survives_whole_not_split() {
        assert_eq!(
            b"Event\r\n".as_slice(),
            strip_trailing_date("Event, Apr 8 2026\r\n").as_bytes()
        );
    }

    /// Case 4 of 4: `"Event, Apr 8 2026\n\n"` is returned unchanged. `$` without
    /// `MULTILINE` sits in front of the **last** terminator only, and the date is
    /// no longer adjacent to it.
    #[test]
    fn jvm_case_4_two_trailing_lfs_leave_the_date_in_place() {
        assert_eq!(
            b"Event, Apr 8 2026\n\n".as_slice(),
            strip_trailing_date("Event, Apr 8 2026\n\n").as_bytes()
        );
    }

    /// The same four cases through C12's public entry points, since that is where
    /// they bite: a title that lost its newline derives a different meeting
    /// filename in C26, and a meeting so misnamed is reclassified as scalable
    /// work.
    #[test]
    fn the_four_jvm_cases_hold_through_the_c12_entry_points() {
        let cases: [(&str, &str); 4] = [
            ("Event, Apr 8 2026", "Event"),
            ("Event, Apr 8 2026\n", "Event\n"),
            ("Event, Apr 8 2026\r\n", "Event\r\n"),
            ("Event, Apr 8 2026\n\n", "Event, Apr 8 2026\n\n"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                expected.as_bytes(),
                clean_task_title(input, "Some Project").as_bytes(),
                "clean_task_title on {input:?}"
            );
            assert_eq!(
                expected.as_bytes(),
                task_title(&[input.to_string()], "Some Project").as_bytes(),
                "task_title on {input:?}"
            );
        }
        // The suffix strip is `removeSuffix`, an exact end-of-string test — it is
        // not terminator-aware the way the regex anchor is. So a description
        // carrying the suffix *and* a trailing newline keeps both: the newline
        // blocks the suffix strip, and the un-stripped suffix then blocks the
        // date strip. Ported as-is; it is the one asymmetry between the two
        // cleanings.
        assert_eq!(
            b"Event".as_slice(),
            clean_task_title("Event, Apr 8 2026 - Some Project", "Some Project").as_bytes()
        );
        assert_eq!(
            b"Event, Apr 8 2026 - Some Project\n".as_slice(),
            clean_task_title("Event, Apr 8 2026 - Some Project\n", "Some Project").as_bytes()
        );
    }

    /// `\d` in a Java regex is ASCII-only unless `UNICODE_CHARACTER_CLASS` is set,
    /// and this pattern does not set it. A port using a Unicode-aware digit class
    /// would strip this.
    #[test]
    fn non_ascii_digits_are_not_a_date() {
        assert_eq!(
            "Event, Sep \u{661}\u{668} 2026",
            strip_trailing_date("Event, Sep \u{661}\u{668} 2026")
        );
    }

    /// `\d{1,2}` is greedy, so a two-digit day is tried first — and the structure
    /// to its left must still line up, which is what rejects a three-digit day.
    #[test]
    fn the_day_is_one_or_two_digits_and_three_is_rejected() {
        assert_eq!("Event", strip_trailing_date("Event, Apr 8 2026"));
        assert_eq!("Event", strip_trailing_date("Event, Apr 18 2026"));
        assert_eq!(
            "Event, Apr 123 2026",
            strip_trailing_date("Event, Apr 123 2026")
        );
    }

    /// The year is exactly four digits and must sit at the very end.
    #[test]
    fn the_year_is_exactly_four_digits_at_the_very_end() {
        assert_eq!(
            "Event, Apr 8 20261",
            strip_trailing_date("Event, Apr 8 20261")
        );
        assert_eq!("Event, Apr 8 202", strip_trailing_date("Event, Apr 8 202"));
    }

    /// The month is one of exactly twelve three-letter names, case-sensitive.
    #[test]
    fn every_month_abbreviation_is_recognised_and_only_those() {
        for month in [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ] {
            assert_eq!(
                "Event",
                strip_trailing_date(&format!("Event, {month} 1 2026")),
                "{month}"
            );
        }
        for not_a_month in ["Jax", "sep", "SEP", "Sept"] {
            let input = format!("Event, {not_a_month} 1 2026");
            assert_eq!(input, strip_trailing_date(&input), "{not_a_month}");
        }
    }

    /// The date strip works on byte indices but must never split a multi-byte
    /// character — a title ending in one, with a date after it, is the case that
    /// would panic on a careless slice.
    #[test]
    fn a_multibyte_title_before_the_date_is_sliced_safely() {
        assert_eq!("Синк", strip_trailing_date("Синк, Sep 18 2026"));
        assert_eq!("\u{1F600}", strip_trailing_date("\u{1F600}, Sep 18 2026"));
    }

    // -----------------------------------------------------------------------
    // Captured baseline fixtures
    // -----------------------------------------------------------------------

    mod baseline {
        //! Verbatim copies of `~/.cache/tt-devpro-rewrite/baseline/`, taken from
        //! the pinned Kotlin incumbent (sha256 c2aef117…) on 2026-09-21. Copied
        //! byte for byte by script rather than retyped, because the under-8h
        //! marker carries an invisible variation selector.

        /// `settle-json.out` — the incumbent's own dump of the nine 2026-09-18
        /// actions.
        pub const SETTLE_JSON: &str = "[\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"Practices - DevPro - Work\",\n            \"totalHours\": 0.5,\n            \"descriptions\": [\n                \"AI Heads Sync\"\n            ],\n            \"devproProjectName\": \"Delivery Practices\",\n            \"billability\": \"NonBillable\"\n        },\n        \"normalizedHours\": 0.5,\n        \"isMeeting\": true,\n        \"isFiller\": false,\n        \"taskTitle\": \"AI Heads Sync\",\n        \"devproProjectId\": \"cf84fdca-4809-4678-98b1-2e7cc56537c0\",\n        \"action\": \"CREATE\"\n    },\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"Practices - DevPro - Work\",\n            \"totalHours\": 0.5,\n            \"descriptions\": [\n                \"SDLC Tools Repository Sync\"\n            ],\n            \"devproProjectName\": \"Delivery Practices\",\n            \"billability\": \"NonBillable\"\n        },\n        \"normalizedHours\": 0.5,\n        \"isMeeting\": true,\n        \"isFiller\": false,\n        \"taskTitle\": \"SDLC Tools Repository Sync\",\n        \"devproProjectId\": \"cf84fdca-4809-4678-98b1-2e7cc56537c0\",\n        \"action\": \"CREATE\"\n    },\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"[borrowed]\",\n            \"totalHours\": 0.5,\n            \"descriptions\": [\n                \"Sync w Ivan about SWE agents\"\n            ],\n            \"devproProjectName\": \"Delivery Practices\",\n            \"billability\": \"NonBillable\"\n        },\n        \"normalizedHours\": 0.5,\n        \"isMeeting\": false,\n        \"isFiller\": false,\n        \"isBorrowed\": true,\n        \"sourceDate\": \"2026-09-14\",\n        \"taskTitle\": \"Sync w Ivan about SWE agents\",\n        \"devproProjectId\": \"cf84fdca-4809-4678-98b1-2e7cc56537c0\",\n        \"action\": \"CREATE\"\n    },\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"Inveniam Measurabl - Presales - DevPro - Work\",\n            \"totalHours\": 1.0,\n            \"descriptions\": [\n                \"Sentinel - next steps\"\n            ],\n            \"devproProjectName\": \"Inveniam SOW #5\",\n            \"billability\": \"Billable\"\n        },\n        \"normalizedHours\": 1.0,\n        \"isMeeting\": true,\n        \"isFiller\": false,\n        \"taskTitle\": \"Sentinel - next steps\",\n        \"devproProjectId\": \"cbcbe09f-0190-4fc4-8691-f59bca797f89\",\n        \"action\": \"CREATE\"\n    },\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"Inveniam Measurabl - Presales - DevPro - Work\",\n            \"totalHours\": 1.0,\n            \"descriptions\": [\n                \"Connect Ingestion tech deep dive\"\n            ],\n            \"devproProjectName\": \"Inveniam SOW #5\",\n            \"billability\": \"Billable\"\n        },\n        \"normalizedHours\": 1.0,\n        \"isMeeting\": true,\n        \"isFiller\": false,\n        \"taskTitle\": \"Connect Ingestion tech deep dive\",\n        \"devproProjectId\": \"cbcbe09f-0190-4fc4-8691-f59bca797f89\",\n        \"action\": \"CREATE\"\n    },\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"[borrowed]\",\n            \"totalHours\": 2.25,\n            \"descriptions\": [\n                \"Map every Connect issue to its producer and its owner\"\n            ],\n            \"devproProjectName\": \"Inveniam SOW #5\",\n            \"billability\": \"Billable\"\n        },\n        \"normalizedHours\": 2.25,\n        \"isMeeting\": false,\n        \"isFiller\": false,\n        \"isBorrowed\": true,\n        \"sourceDate\": \"2026-09-17\",\n        \"taskTitle\": \"Map every Connect issue to its producer and its owner\",\n        \"devproProjectId\": \"cbcbe09f-0190-4fc4-8691-f59bca797f89\",\n        \"action\": \"CREATE\"\n    },\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"[borrowed]\",\n            \"totalHours\": 0.5,\n            \"descriptions\": [\n                \"Analyze the Cursor usage export and build the token-governance case\"\n            ],\n            \"devproProjectName\": \"Inveniam SOW #5\",\n            \"billability\": \"Billable\"\n        },\n        \"normalizedHours\": 0.5,\n        \"isMeeting\": false,\n        \"isFiller\": false,\n        \"isBorrowed\": true,\n        \"sourceDate\": \"2026-09-15\",\n        \"taskTitle\": \"Analyze the Cursor usage export and build the token-governance case\",\n        \"devproProjectId\": \"cbcbe09f-0190-4fc4-8691-f59bca797f89\",\n        \"action\": \"CREATE\"\n    },\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"[borrowed]\",\n            \"totalHours\": 0.5,\n            \"descriptions\": [\n                \"Program management discussion\"\n            ],\n            \"devproProjectName\": \"Inveniam SOW #5\",\n            \"billability\": \"Billable\"\n        },\n        \"normalizedHours\": 0.5,\n        \"isMeeting\": false,\n        \"isFiller\": false,\n        \"isBorrowed\": true,\n        \"sourceDate\": \"2026-09-15\",\n        \"taskTitle\": \"Program management discussion\",\n        \"devproProjectId\": \"cbcbe09f-0190-4fc4-8691-f59bca797f89\",\n        \"action\": \"CREATE\"\n    },\n    {\n        \"aggregate\": {\n            \"date\": \"2026-09-18\",\n            \"chronoProject\": \"[borrowed]\",\n            \"totalHours\": 0.25,\n            \"descriptions\": [\n                \"Send Omar the AWS IAM user and verify Snowflake and S3 access\"\n            ],\n            \"devproProjectName\": \"Inveniam SOW #5\",\n            \"billability\": \"Billable\"\n        },\n        \"normalizedHours\": 0.25,\n        \"isMeeting\": false,\n        \"isFiller\": false,\n        \"isBorrowed\": true,\n        \"sourceDate\": \"2026-09-14\",\n        \"taskTitle\": \"Send Omar the AWS IAM user and verify Snowflake and S3 access\",\n        \"devproProjectId\": \"cbcbe09f-0190-4fc4-8691-f59bca797f89\",\n        \"action\": \"CREATE\"\n    }\n]\n";

        /// `settle-dryrun.out` — the same run's rendered stdout.
        pub const SETTLE_DRYRUN: &str = "2026-09-18 Fri — 9 entries → 7.00h\n  Delivery Practices  Meeting   0.50  Create  \"AI Heads Sync\"\n  Delivery Practices  Meeting   0.50  Create  \"SDLC Tools Repository Sync\"\n  Delivery Practices  Work      0.50  Create  \"Sync w Ivan about SWE agents\"\n  Inveniam SOW #5     Meeting   1.00  Create  \"Sentinel - next steps\"\n  Inveniam SOW #5     Meeting   1.00  Create  \"Connect Ingestion tech deep dive\"\n  Inveniam SOW #5     Work      2.25  Create  \"Map every Connect issue to its producer and its owner\"\n  Inveniam SOW #5     Work      0.50  Create  \"Analyze the Cursor usage export and build the token-governance case\"\n  Inveniam SOW #5     Work      0.50  Create  \"Program management discussion\"\n  Inveniam SOW #5     Work      0.25  Create  \"Send Omar the AWS IAM user and verify Snowflake and S3 access\"\n\nTotal: 7.00h across 1 day, 9 entries\n\n⚠️  Under 8h (borrowed+filler cap reached):\n  2026-09-18: 7.00h (need 1.00h more)\n";

        /// `settle-range-sep-dryrun.out` — eleven fully settled days.
        pub const SETTLE_RANGE_DRYRUN: &str = "2026-09-01 Tue — 7 entries → 8.00h\n  AI Practices        Meeting   0.50  Update  \"AI Team Sync\"\n  HR Management       Meeting   0.50  Update  \"Performance Review with Yana Kapatsila\"\n  Inveniam SOW #5     Meeting   1.00  Update  \"Measurabl x Inveniam - Workstream Sync\"\n  Inveniam SOW #5     Meeting   0.50  Update  \"D3 - Agentic DQ - Sync\"\n  Inveniam SOW #5     Work      4.50  Update  \"Review and polish the agent architecture diagram\"\n  Inveniam SOW #5     Work      0.75  Update  \"Process the 2026-09-01 materials batch\"\n  Inveniam SOW #5     Work      0.25  Update  \"Salesforce data migration discussion\"\n\n2026-09-02 Wed — 4 entries → 8.00h\n  Inveniam SOW #5     Meeting   0.75  Update  \"AICO: Daily Standup\"\n  Inveniam SOW #5     Work      1.00  Update  \"Yurii, architecture sync\"\n  Inveniam SOW #5     Work      5.25  Update  \"Review and polish the agent architecture diagram\"\n  Inveniam SOW #5     Work      1.00  Update  \"Check whether Snowflake access landed and push if not\"\n\n2026-09-03 Thu — 8 entries → 8.00h\n  Delivery Practices  Meeting   0.50  Update  \"AI Practice Daily\"\n  Delivery Practices  Meeting   0.50  Update  \"Heads Sync\"\n  HR Management       Meeting   0.50  Update  \"MBO goals review\"\n  Inveniam SOW #5     Meeting   1.00  Update  \"Weekly Measurabl.ai Check-in\"\n  Inveniam SOW #5     Meeting   0.50  Update  \"D3 - Agentic DQ - Sync\"\n  Inveniam SOW #5     Work      3.00  Update  \"Agentic AI for DQ high-level architecture\"\n  Inveniam SOW #5     Work      1.25  Update  \"New tracks scope and leadership\"\n  Inveniam SOW #5     Work      0.75  Update  \"Program management discussions\"\n\n2026-09-04 Fri — 5 entries → 8.00h\n  AI Practices        Meeting   0.50  Update  \"Goals\"\n  Delivery Practices  Meeting   0.50  Update  \"AI Heads Sync\"\n  Inveniam SOW #5     Meeting   0.75  Update  \"Yurii / Kseniia\"\n  Inveniam SOW #5     Work      5.25  Update  \"Map every Connect issue to its producer and its owner\"\n  Inveniam SOW #5     Work      1.00  Update  \"Analyze the Cursor usage export and build the token-governance case\"\n\n2026-09-07 Mon — 1 entry → 8.00h\n  AI Practices        Work      8.00  Create  \"Handle incoming communication\"\n\n2026-09-08 Tue — 5 entries → 8.00h\n  AI Practices        Meeting   0.50  Update  \"AI Team Sync\"\n  AI Practices        Work      5.50  Update  \"Request a Security exception for ProtonVPN and Brave, remove the rest\"\n  Delivery Practices  Meeting   0.50  Update  \"SWE agentic sync\"\n  Inveniam SOW #5     Meeting   0.50  Update  \"Agentic AI for Data Quality - KT - Inveniam\"\n  Inveniam SOW #5     Meeting   1.00  Update  \"D3 - Agentic DQ - Sync\"\n\n2026-09-09 Wed — 8 entries → 8.00h\n  AI Practices        Work      2.25  Update  \"Respond to Ivan on SECSM-4546 ProtonVPN exception\"\n  Delivery Practices  Meeting   0.50  Update  \"Yurii - Denys\"\n  Delivery Practices  Meeting   0.50  Update  \"AI in Practices - SDD standartization\"\n  Delivery Practices  Meeting   0.50  Update  \"Yurii - Maksym\"\n  Inveniam SOW #5     Meeting   0.75  Update  \"Agentic AI for Data Quality - KT - Inveniam, part 2\"\n  Inveniam SOW #5     Meeting   0.75  Update  \"AICO: Weekly Standup\"\n  Inveniam SOW #5     Work      2.25  Update  \"Analyze the Cursor usage export and build the token-governance case\"\n  Presales            Meeting   0.50  Update  \"AI in Proposals & Practices Involvement\"\n\n2026-09-10 Thu — 11 entries → 8.00h\n  AI Practices        Work      0.25  Update  \"Respond to Ivan on SECSM-4546 ProtonVPN exception\"\n  AI Practices        Work      0.75  Update  \"Answer Ivan on whether the code-review agent effectiveness was measured\"\n  Delivery Practices  Meeting   0.50  Update  \"Heads Sync\"\n  Inveniam SOW #5     Meeting   0.50  Update  \"Yurii/Daria\"\n  Inveniam SOW #5     Meeting   1.00  Update  \"Weekly Measurabl.ai Check-in\"\n  Inveniam SOW #5     Meeting   0.75  Update  \"D3 - Agentic DQ - Sync\"\n  Inveniam SOW #5     Work      0.75  Update  \"Map every Connect issue to its producer and its owner\"\n  Inveniam SOW #5     Work      0.75  Update  \"Program initiaitives discussion\"\n  Inveniam SOW #5     Work      1.00  Update  \"Prep the Agentic AI for Data Quality solution for the D3 sync\"\n  Inveniam SOW #5     Work      1.00  Update  \"Agentic DQ, status update report\"\n  Presales            Work      0.75  Update  \"Scope the Epos demo call taken over from Naun\"\n\n2026-09-11 Fri — 2 entries → 8.00h\n  Delivery Practices  Meeting   0.50  Update  \"AI Heads Sync\"\n  Inveniam SOW #5     Work      7.50  Update  \"Map every Connect issue to its producer and its owner\"\n\n2026-09-14 Mon — 5 entries → 8.00h\n  Delivery Practices  Meeting   0.25  Update  \"AI Practice Daily\"\n  Delivery Practices  Work      1.00  Update  \"Sync w Ivan about SWE agents\"\n  Inveniam SOW #5     Work      4.25  Update  \"Analyze the Cursor usage export and build the token-governance case\"\n  Inveniam SOW #5     Work      2.00  Update  \"Map every Connect issue to its producer and its owner\"\n  Inveniam SOW #5     Work      0.50  Update  \"Send Omar the AWS IAM user and verify Snowflake and S3 access\"\n\n2026-09-15 Tue — 7 entries → 8.00h\n  AI Practices        Meeting   0.50  Update  \"1-1 Yurii/Irina\"\n  AI Practices        Meeting   0.50  Update  \"AI Team Sync\"\n  HR Management       Work      1.00  Update  \"MBO\"\n  Inveniam SOW #5     Meeting   0.50  Update  \"Cursor AI usage strategy & issues\"\n  Inveniam SOW #5     Work      2.25  Update  \"Analyze the Cursor usage export and build the token-governance case\"\n  Inveniam SOW #5     Work      2.25  Update  \"Program management discussion\"\n  Inveniam SOW #5     Work      1.00  Update  \"Map every Connect issue to its producer and its owner\"\n\nTotal: 88.00h across 11 days, 63 entries\n";
    }
}
