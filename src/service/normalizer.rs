//! Port of `service/TimeNormalizer.kt` — C5 (8h normalization) and C26 (meeting
//! detection). The Kotlin object has no tests at all (G1), so every test below is new.
//!
//! Three things here are literal translations of JVM semantics rather than the
//! idiomatic Rust, and each one was measured against GraalVM JDK 21 before being
//! written down:
//!
//! * **Quantization rounds, it does not truncate** (C27). `TimeNormalizer.kt:149` is
//!   one of three textually identical `(hours / 0.25).roundToInt() * 0.25` copies;
//!   the five `toInt()` sites in `SettleCommand.kt` truncate instead, and both
//!   regimes are contracts. `0.375` becomes `0.5` here and `0.25` there.
//! * **`roundToInt()` is Java's `Math.round`, which is not `(x + 0.5).floor()`.**
//!   `javap -p -c` on kotlin-stdlib 1.9.22 `MathKt__MathJVMKt.roundToInt(double)`
//!   shows NaN → `IllegalArgumentException`, saturation at the **32-bit** bounds,
//!   then `Math.round`. And `Math.round(0.49999999999999994)` is `0` on the JVM
//!   while `floor(0.49999999999999994 + 0.5)` is `1` — measured, not cited. So the
//!   bit-exact algorithm is reproduced below; see [`round_to_quarter`].
//! * **Java's `$` matches before a trailing line terminator, and `replaceAll` keeps
//!   that terminator.** `"Event, Apr 8 2026\n"` becomes `"Event\n"` on the JVM, not
//!   `"Event"`. A Rust regex anchored `(?:\r?\n)?$` with an empty replacement would
//!   eat the newline and derive a different meeting filename, so the date-suffix
//!   strip is hand-rolled to keep the terminator.
//!
//! Two silent swallows are preserved deliberately: a knowledge base that cannot be
//! walked yields an empty directory list, which makes **every** entry a non-meeting
//! rather than an error (`TimeNormalizer.kt:21-29`), and a mid-walk I/O error
//! discards the directories already found rather than returning them — Kotlin's
//! `try` wraps the terminal `.toList()`, so the `UncheckedIOException` takes the
//! whole stream with it (measured).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::model::{DayProjectAggregate, NormalizedAggregate};

/// `TimeNormalizer.kt:14`.
const TARGET_HOURS: f64 = 8.0;

/// `TimeNormalizer.kt:15`. Public because `FillerService.kt:165` and
/// `BorrowerService.kt:175` are the other two copies of the same constant and the
/// same helper, and the plan expects the three to collapse into this one.
pub const HOUR_INCREMENT: f64 = 0.25;

/// `TimeNormalizer.kt:23`. `Files.walk(start, 10)` visits the start itself at depth
/// 0 and descends while depth < 10, so a `Calendar` directory whose path relative to
/// the root has ten components is found and one with eleven is not (measured).
const MAX_WALK_DEPTH: usize = 10;

const MONTHS: [&[u8]; 12] = [
    b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov", b"Dec",
];

/// Characters `sanitizeRegex` (`TimeNormalizer.kt:31`) replaces with `-`.
const HOSTILE_CHARS: [char; 9] = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Ports the `TimeNormalizer` object. Kotlin holds `calendarDirs` in a `by lazy`
/// computed once per process from a constant path; the plan makes the knowledge-base
/// root a parameter with the same default so C26 becomes testable, which is the only
/// reason this is a struct rather than free functions.
pub struct TimeNormalizer {
    calendar_dirs: Vec<PathBuf>,
}

impl TimeNormalizer {
    /// `TimeNormalizer.kt:19` — `${System.getProperty("user.home")}/knowledge-base`.
    pub fn new() -> Self {
        Self::with_knowledge_base(default_knowledge_base())
    }

    pub fn with_knowledge_base(root: impl AsRef<Path>) -> Self {
        Self {
            calendar_dirs: find_calendar_dirs(root.as_ref()),
        }
    }

    /// Every directory named `Calendar` the walk found, in walk order.
    pub fn calendar_dirs(&self) -> &[PathBuf] {
        &self.calendar_dirs
    }

    /// `TimeNormalizer.normalize` (`TimeNormalizer.kt:40-47`). Groups by date in
    /// first-encounter order (Kotlin's `groupBy` is a `LinkedHashMap`), normalizes
    /// each day, then stably sorts the concatenation by `(date, devproProjectName)`.
    pub fn normalize(&self, aggregates: &[DayProjectAggregate]) -> Vec<NormalizedAggregate> {
        let mut groups: Vec<Vec<&DayProjectAggregate>> = Vec::new();
        for agg in aggregates {
            match groups.iter_mut().find(|g| g[0].date == agg.date) {
                Some(group) => group.push(agg),
                None => groups.push(vec![agg]),
            }
        }

        let mut out: Vec<NormalizedAggregate> = Vec::new();
        for group in &groups {
            out.extend(self.normalize_day(group));
        }
        out.sort_by(by_date_then_project);
        out
    }

    /// `TimeNormalizer.normalizeDay` (`TimeNormalizer.kt:49-109`).
    fn normalize_day(&self, aggregates: &[&DayProjectAggregate]) -> Vec<NormalizedAggregate> {
        // :51-54 — flag first, hours untouched.
        let with_meeting_flag: Vec<NormalizedAggregate> = aggregates
            .iter()
            .map(|agg| NormalizedAggregate {
                original: (*agg).clone(),
                normalized_hours: agg.total_hours,
                is_meeting: self.is_meeting_entry(agg),
            })
            .collect();

        // :57-58 — fixed = meetings OR anything carrying a cap.
        let fixed_entries: Vec<NormalizedAggregate> = with_meeting_flag
            .iter()
            .filter(|e| e.is_meeting || e.original.max_hours.is_some())
            .cloned()
            .collect();
        let fixed_hours = sum_hours(&fixed_entries);

        // :61-63.
        let work_entries: Vec<NormalizedAggregate> = with_meeting_flag
            .iter()
            .filter(|e| !e.is_meeting && e.original.max_hours.is_none())
            .cloned()
            .collect();
        let work_hours = sum_hours(&work_entries);
        let total_hours = fixed_hours + work_hours;

        // :66-68 — already 8h within half an increment: round everything and stop.
        // Note what this branch does *not* do: no 0.25h floor, and no final sort.
        if (total_hours - TARGET_HOURS).abs() < HOUR_INCREMENT / 2.0 {
            return rounded(&with_meeting_flag);
        }

        // :71.
        let target_work_hours = TARGET_HOURS - fixed_hours;

        // :74-76.
        if work_entries.is_empty() || target_work_hours <= 0.0 {
            return rounded(&with_meeting_flag);
        }

        // :79.
        let scale_factor = target_work_hours / work_hours;

        // :82-85 — scale, round, floor at one increment so nothing lands on zero.
        let scaled_work: Vec<NormalizedAggregate> = work_entries
            .iter()
            .map(|entry| {
                let scaled = entry.normalized_hours * scale_factor;
                let mut copy = entry.clone();
                copy.normalized_hours = java_max(HOUR_INCREMENT, round_to_quarter(scaled));
                copy
            })
            .collect();

        // :89-95.
        let scaled_work_total = sum_hours(&scaled_work);
        let rounded_fixed = rounded(&fixed_entries);
        let rounded_fixed_total = sum_hours(&rounded_fixed);
        let adjusted_target = TARGET_HOURS - rounded_fixed_total;
        let diff = adjusted_target - scaled_work_total;

        // :97-105 — the whole residual goes to the largest scalable entry.
        //
        // C28: `sortedByDescending {}.first()` is a *stable* TimSort, so a tie at the
        // top keeps the entry seen first. Rust's `max_by` returns the **last**
        // maximum and would move hours to a different project on an ordinary day, so
        // this is a stable `sort_by` with the comparator reversed, then `[0]`.
        let final_work = if diff.abs() >= HOUR_INCREMENT / 2.0 && !scaled_work.is_empty() {
            let mut sorted = scaled_work;
            sorted.sort_by(|a, b| b.normalized_hours.total_cmp(&a.normalized_hours));
            let mut adjusted = sorted[0].clone();
            adjusted.normalized_hours = java_max(
                HOUR_INCREMENT,
                round_to_quarter(adjusted.normalized_hours + diff),
            );
            let mut out = vec![adjusted];
            out.extend(sorted.into_iter().skip(1));
            out
        } else {
            scaled_work
        };

        // :107-108 — fixed entries lead the concatenation, so a `(date, project)` tie
        // between a fixed and a scaled row puts the fixed one first whatever the
        // input order was. The early-return branches above keep the input order.
        let mut out = rounded_fixed;
        out.extend(final_work);
        out.sort_by(by_date_then_project);
        out
    }

    /// `TimeNormalizer.isMeetingEntry` (`TimeNormalizer.kt:111-145`) — C26.
    pub fn is_meeting_entry(&self, agg: &DayProjectAggregate) -> bool {
        // :113-115 — admin work is a meeting unconditionally, before any I/O.
        if agg.chrono_project.starts_with("Operations -") {
            return true;
        }

        // :117-119.
        let Some(raw_description) = agg.descriptions.first() else {
            return false;
        };

        // :123-129.
        let project_suffix = format!(" - {}", agg.chrono_project);
        let meeting_name = strip_date_suffix(remove_suffix(raw_description, &project_suffix));
        let sanitized = sanitize(&meeting_name);

        // :130-134 — the middle candidate's double space is load-bearing: it catches a
        // trailing space in the upstream `display_name`. Deleting it reclassifies real
        // meetings as scalable work.
        let date = agg.date.to_string();
        let candidates = [
            format!("{sanitized} {date}.md"),
            format!("{sanitized}  {date}.md"),
            format!("{sanitized}.md"),
        ];

        // :136-144.
        for dir in &self.calendar_dirs {
            for filename in &candidates {
                if dir.join(filename).exists() {
                    return true;
                }
            }
        }

        false
    }
}

impl Default for TimeNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

/// `TimeNormalizer.kt:147-150` — the rounding half of C27, shared by the three
/// identical copies the plan collapses into one.
///
/// Not `(x / 0.25).round() * 0.25`: `f64::round` sends halves away from zero and
/// Kotlin's sends them toward `+∞`, so `-0.375` would become `-0.5` instead of
/// `-0.25`. Not `((x / 0.25) + 0.5).floor() * 0.25` either — that agrees with the
/// JVM everywhere except where the `+ 0.5` itself rounds up, and
/// `Math.round(0.49999999999999994) == 0` while `floor(… + 0.5) == 1`.
///
/// # Panics
///
/// On NaN, reproducing `roundToInt`'s `IllegalArgumentException`. This is reachable:
/// a day whose scalable entries all have zero hours divides by zero, and `0.0 * ∞`
/// is NaN.
pub fn round_to_quarter(hours: f64) -> f64 {
    f64::from(kotlin_round_to_int(hours / HOUR_INCREMENT)) * HOUR_INCREMENT
}

/// `kotlin.math.roundToInt(Double)`, read off `javap -p -c` of kotlin-stdlib 1.9.22
/// `MathKt__MathJVMKt`: NaN throws, then saturation at the **32-bit** bounds (not
/// 64-bit), then `Math.round`.
fn kotlin_round_to_int(x: f64) -> i32 {
    if x.is_nan() {
        panic!("Cannot round NaN value.");
    }
    if x > f64::from(i32::MAX) {
        return i32::MAX;
    }
    if x < f64::from(i32::MIN) {
        return i32::MIN;
    }
    java_math_round(x) as i32
}

/// `java.lang.Math.round(double)`, transcribed from the JDK. Exact round-half-up on
/// the bit pattern, which is why it differs from `(x + 0.5).floor()`.
fn java_math_round(a: f64) -> i64 {
    const SIGNIFICAND_WIDTH: i64 = 53;
    const EXP_BIAS: i64 = 1023;
    const EXP_BIT_MASK: u64 = 0x7ff0_0000_0000_0000;
    const SIGNIF_BIT_MASK: u64 = 0x000f_ffff_ffff_ffff;

    let long_bits = a.to_bits() as i64;
    let biased_exp = ((a.to_bits() & EXP_BIT_MASK) >> (SIGNIFICAND_WIDTH - 1)) as i64;
    let shift = (SIGNIFICAND_WIDTH - 2 + EXP_BIAS) - biased_exp;
    if (shift & -64) == 0 {
        // shift >= 0 && shift < 64
        let mut r = ((a.to_bits() & SIGNIF_BIT_MASK) | (SIGNIF_BIT_MASK + 1)) as i64;
        if long_bits < 0 {
            r = -r;
        }
        ((r >> shift) + 1) >> 1
    } else {
        // Java's narrowing `(long)` cast saturates and maps NaN to 0, and so does
        // Rust's `as`.
        a as i64
    }
}

/// `kotlin.comparisons.maxOf(Double, Double)` is `Math.max`, which propagates NaN and
/// orders `-0.0` below `0.0`. `f64::max` does neither.
fn java_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { b } else { a };
    }
    if a > b { a } else { b }
}

fn sum_hours(entries: &[NormalizedAggregate]) -> f64 {
    entries.iter().map(|e| e.normalized_hours).sum()
}

fn rounded(entries: &[NormalizedAggregate]) -> Vec<NormalizedAggregate> {
    entries
        .iter()
        .map(|e| {
            let mut copy = e.clone();
            copy.normalized_hours = round_to_quarter(copy.normalized_hours);
            copy
        })
        .collect()
}

/// `compareBy({ it.original.date }, { it.original.devproProjectName })`
/// (`TimeNormalizer.kt:46,108`).
fn by_date_then_project(a: &NormalizedAggregate, b: &NormalizedAggregate) -> Ordering {
    a.original.date.cmp(&b.original.date).then_with(|| {
        a.original
            .devpro_project_name
            .cmp(&b.original.devpro_project_name)
    })
}

fn default_knowledge_base() -> PathBuf {
    // `System.getProperty("user.home")` is always set on the JVM. If the home
    // directory cannot be resolved here, the walk below fails and the swallow at
    // `TimeNormalizer.kt:26` turns that into "no meetings", exactly as a missing
    // knowledge base already does.
    dirs::home_dir().unwrap_or_default().join("knowledge-base")
}

/// `TimeNormalizer.kt:21-29`. Any failure — a missing root, an unreadable
/// subdirectory partway through — yields an empty list, never an error.
fn find_calendar_dirs(root: &Path) -> Vec<PathBuf> {
    walk_for_calendar_dirs(root).unwrap_or_default()
}

fn walk_for_calendar_dirs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    // `Files.walk` reads the start's attributes first and throws if it cannot.
    let start = std::fs::symlink_metadata(root)?;
    let mut out = Vec::new();
    collect_calendar_dirs(root, start.is_dir(), 0, &mut out)?;
    Ok(out)
}

/// `Files.walk` without `FOLLOW_LINKS` does not descend through a symlink, while the
/// `Files.isDirectory` in the filter *does* follow it — so a symlink named `Calendar`
/// that points at a directory matches but is not walked into. Both halves measured.
fn collect_calendar_dirs(
    path: &Path,
    descendable: bool,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    if path.file_name().is_some_and(|name| name == "Calendar") && path.is_dir() {
        out.push(path.to_path_buf());
    }
    if !descendable || depth == MAX_WALK_DEPTH {
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let child_is_real_dir = entry.file_type()?.is_dir();
        collect_calendar_dirs(&entry.path(), child_is_real_dir, depth + 1, out)?;
    }
    Ok(())
}

/// `kotlin.text.removeSuffix` — drops the suffix once, only at the end.
fn remove_suffix<'a>(s: &'a str, suffix: &str) -> &'a str {
    s.strip_suffix(suffix).unwrap_or(s)
}

/// `sanitizeRegex` (`TimeNormalizer.kt:31`), every occurrence.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if HOSTILE_CHARS.contains(&c) { '-' } else { c })
        .collect()
}

/// `dateSuffixRegex` (`TimeNormalizer.kt:32`) applied through `replace(regex, "")`.
///
/// Java's `$` without `MULTILINE` matches at end of input *and* just before a single
/// trailing line terminator, and the replacement leaves that terminator in place:
/// `"Event, Apr 8 2026\n"` → `"Event\n"` (measured). Two trailing terminators means
/// no match at all, because `$` only reaches back over the last one.
fn strip_date_suffix(s: &str) -> String {
    let (core, terminator) = split_final_line_terminator(s);
    match date_suffix_start(core) {
        Some(at) => format!("{}{}", &core[..at], terminator),
        None => s.to_string(),
    }
}

fn split_final_line_terminator(s: &str) -> (&str, &str) {
    for terminator in ["\r\n", "\n", "\r", "\u{85}", "\u{2028}", "\u{2029}"] {
        if let Some(core) = s.strip_suffix(terminator) {
            return (core, &s[core.len()..]);
        }
    }
    (s, "")
}

/// Byte offset where a trailing `, Mon D YYYY` begins, if `core` ends with one.
///
/// The regex is `, (Jan|…|Dec) \d{1,2} \d{4}$`, and `\d` there is ASCII `[0-9]`.
/// The greedy `\d{1,2}` needs no backtracking search here: if two trailing day digits
/// are not preceded by a space, one cannot be either, since the character before it is
/// the other digit.
fn date_suffix_start(core: &str) -> Option<usize> {
    let b = core.as_bytes();

    // `\d{4}` at the end.
    let year_start = b.len().checked_sub(4)?;
    if !b[year_start..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    // The space before the year. A fifth digit would sit here instead.
    if year_start == 0 || b[year_start - 1] != b' ' {
        return None;
    }

    // `\d{1,2}` for the day.
    let day_end = year_start - 1;
    let mut day_len = 0usize;
    while day_len < 2 && day_end > day_len && b[day_end - 1 - day_len].is_ascii_digit() {
        day_len += 1;
    }
    if day_len == 0 {
        return None;
    }
    let day_start = day_end - day_len;
    if day_start == 0 || b[day_start - 1] != b' ' {
        return None;
    }

    // The three-letter month, and the `", "` in front of it.
    let month_end = day_start - 1;
    let month_start = month_end.checked_sub(3)?;
    if !MONTHS.contains(&&b[month_start..month_end]) {
        return None;
    }
    let comma = month_start.checked_sub(2)?;
    if b[comma] != b',' || b[comma + 1] != b' ' {
        return None;
    }
    Some(comma)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::fs;
    use tempfile::TempDir;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    /// A scalable work entry: not an `Operations -` project, no cap, and a description
    /// that will not match anything on disk.
    fn work(project: &str, hours: f64) -> DayProjectAggregate {
        DayProjectAggregate {
            date: date(2026, 9, 18),
            chrono_project: "Velocitor - DevPro - Work".to_string(),
            total_hours: hours,
            descriptions: vec![format!("work on {project}")],
            devpro_project_name: project.to_string(),
            billability: "Billable".to_string(),
            max_hours: None,
        }
    }

    /// A meeting by the `Operations -` short circuit, so no filesystem is involved.
    fn meeting(project: &str, hours: f64) -> DayProjectAggregate {
        DayProjectAggregate {
            chrono_project: "Operations - DevPro - Work".to_string(),
            ..work(project, hours)
        }
    }

    fn capped(project: &str, hours: f64, cap: f64) -> DayProjectAggregate {
        DayProjectAggregate {
            max_hours: Some(cap),
            ..work(project, hours)
        }
    }

    /// No knowledge base on disk, so `calendarDirs` is empty and only the
    /// `Operations -` short circuit can produce a meeting.
    fn offline() -> TimeNormalizer {
        TimeNormalizer::with_knowledge_base("/definitely/not/a/knowledge/base")
    }

    fn hours_by_project(result: &[NormalizedAggregate]) -> Vec<(String, f64)> {
        result
            .iter()
            .map(|e| (e.original.devpro_project_name.clone(), e.normalized_hours))
            .collect()
    }

    /// Builds a knowledge base whose files are given as paths relative to the root.
    fn knowledge_base(files: &[&str]) -> TempDir {
        let dir = TempDir::new().expect("temp dir");
        for file in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            fs::write(&path, "").expect("write");
        }
        dir
    }

    fn calendar_entry(description: &str) -> DayProjectAggregate {
        DayProjectAggregate {
            descriptions: vec![description.to_string()],
            ..work("Delivery Practices", 0.5)
        }
    }

    // -----------------------------------------------------------------------
    // C26 — meeting detection. `TimeNormalizer.kt:111-145`.
    // -----------------------------------------------------------------------

    /// C26.1, `TimeNormalizer.kt:113-115`. The short circuit returns before any
    /// filesystem access — proved by pointing the normalizer at a root that does not
    /// exist and giving the entry no descriptions at all, so nothing else could say
    /// "meeting".
    #[test]
    fn an_operations_project_is_a_meeting_without_touching_the_filesystem() {
        let entry = DayProjectAggregate {
            descriptions: vec![],
            ..meeting("Operations", 1.0)
        };
        assert!(offline().is_meeting_entry(&entry));
    }

    /// C26.1. `startsWith`, not `contains` — a project merely mentioning Operations
    /// is ordinary scalable work.
    #[test]
    fn a_project_that_only_contains_operations_is_not_a_meeting() {
        let entry = DayProjectAggregate {
            chrono_project: "DevPro Operations - Work".to_string(),
            ..work("P", 1.0)
        };
        assert!(!offline().is_meeting_entry(&entry));
    }

    /// C26.1. The prefix includes the trailing `" -"`; `"Operations Team"` is a
    /// different project.
    #[test]
    fn the_operations_prefix_includes_its_trailing_dash() {
        let entry = DayProjectAggregate {
            chrono_project: "Operations Team - DevPro - Work".to_string(),
            ..work("P", 1.0)
        };
        assert!(!offline().is_meeting_entry(&entry));
    }

    /// C26.2, `TimeNormalizer.kt:117-119`. With no description there is no filename to
    /// derive, so the answer is false even though the calendar directory exists.
    #[test]
    fn an_entry_with_no_descriptions_is_not_a_meeting() {
        let kb = knowledge_base(&["Work/Calendar/AI Heads Sync 2026-09-18.md"]);
        let entry = DayProjectAggregate {
            descriptions: vec![],
            ..work("Delivery Practices", 0.5)
        };
        assert!(!TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    /// C26.4, first candidate (`TimeNormalizer.kt:131`).
    #[test]
    fn the_first_candidate_is_the_name_followed_by_one_space_and_the_date() {
        let kb = knowledge_base(&["Work/Calendar/AI Heads Sync 2026-09-18.md"]);
        let entry = calendar_entry("AI Heads Sync");
        assert!(TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    /// C26.4, second candidate (`TimeNormalizer.kt:132`). The double space is the one
    /// that looks like a typo: only the two-space file exists here, so deleting that
    /// candidate turns a real meeting into scalable work.
    #[test]
    fn the_double_space_candidate_catches_a_trailing_space_in_the_display_name() {
        let kb = knowledge_base(&["Work/Calendar/AI Heads Sync  2026-09-18.md"]);
        assert!(
            !kb.path()
                .join("Work/Calendar/AI Heads Sync 2026-09-18.md")
                .exists()
        );
        let entry = calendar_entry("AI Heads Sync");
        assert!(TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    /// C26.4, third candidate (`TimeNormalizer.kt:133`) — the description already
    /// carries its own date, so no date is appended.
    #[test]
    fn the_third_candidate_is_the_bare_name_with_no_date_appended() {
        let kb = knowledge_base(&["Work/Calendar/AI Heads Sync.md"]);
        let entry = calendar_entry("AI Heads Sync");
        assert!(TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    /// C26.3, `TimeNormalizer.kt:125-127`. The project suffix comes off the end.
    #[test]
    fn the_project_suffix_is_stripped_from_the_end_of_the_description() {
        let kb = knowledge_base(&["Work/Calendar/AI Heads Sync 2026-09-18.md"]);
        let entry = DayProjectAggregate {
            chrono_project: "Practices - DevPro - Work".to_string(),
            descriptions: vec!["AI Heads Sync - Practices - DevPro - Work".to_string()],
            ..work("Delivery Practices", 0.5)
        };
        assert!(TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    /// C26.3. `removeSuffix` is not `replace`: the same text in the middle stays. The
    /// second half is what makes this falsifiable — a port that used `replace` would
    /// find the stripped file and pass the first assertion alone.
    #[test]
    fn a_project_suffix_in_the_middle_of_the_description_is_not_stripped() {
        let entry = DayProjectAggregate {
            chrono_project: "Practices - DevPro - Work".to_string(),
            descriptions: vec!["AI - Practices - DevPro - Work Sync".to_string()],
            ..work("Delivery Practices", 0.5)
        };

        let intact =
            knowledge_base(&["Calendar/AI - Practices - DevPro - Work Sync 2026-09-18.md"]);
        assert!(TimeNormalizer::with_knowledge_base(intact.path()).is_meeting_entry(&entry));

        let stripped = knowledge_base(&["Calendar/AI  Sync 2026-09-18.md"]);
        assert!(!TimeNormalizer::with_knowledge_base(stripped.path()).is_meeting_entry(&entry));
    }

    /// C26.3, `TimeNormalizer.kt:128` — the old description format.
    #[test]
    fn a_trailing_date_suffix_is_stripped_from_the_meeting_name() {
        let kb = knowledge_base(&["Calendar/Team Sync 2026-04-08.md"]);
        let entry = DayProjectAggregate {
            date: date(2026, 4, 8),
            descriptions: vec!["Team Sync, Apr 8 2026".to_string()],
            ..work("Delivery Practices", 0.5)
        };
        assert!(TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    /// C26.3. Both strips run, in that order: project suffix first, then date.
    #[test]
    fn the_date_suffix_is_stripped_after_the_project_suffix() {
        let kb = knowledge_base(&["Calendar/Team Sync 2026-04-08.md"]);
        let entry = DayProjectAggregate {
            date: date(2026, 4, 8),
            chrono_project: "Practices - DevPro - Work".to_string(),
            descriptions: vec!["Team Sync, Apr 8 2026 - Practices - DevPro - Work".to_string()],
            ..work("Delivery Practices", 0.5)
        };
        assert!(TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    /// Java's `$` reaches back over one trailing line terminator but `replaceAll`
    /// leaves the terminator in the result: measured on GraalVM JDK 21,
    /// `"Event, Apr 8 2026\n"` → `"Event\n"`. A Rust regex anchored `(?:\r?\n)?$` with
    /// an empty replacement yields `"Event"` and probes a different filename, so the
    /// second half of this test is the one that fails under that recipe.
    #[test]
    fn a_stripped_date_suffix_leaves_a_trailing_newline_in_the_meeting_name() {
        let entry = DayProjectAggregate {
            date: date(2026, 4, 8),
            descriptions: vec!["Team Sync, Apr 8 2026\n".to_string()],
            ..work("Delivery Practices", 0.5)
        };

        let with_newline = knowledge_base(&["Calendar/Team Sync\n 2026-04-08.md"]);
        assert!(TimeNormalizer::with_knowledge_base(with_newline.path()).is_meeting_entry(&entry));

        let without_newline = knowledge_base(&["Calendar/Team Sync 2026-04-08.md"]);
        assert!(
            !TimeNormalizer::with_knowledge_base(without_newline.path()).is_meeting_entry(&entry)
        );
    }

    /// Two trailing terminators put the date out of `$`'s reach, so nothing is
    /// stripped (measured: `"Event, Apr 8 2026\n\n"` comes back unchanged).
    #[test]
    fn two_trailing_newlines_put_the_date_suffix_out_of_reach() {
        assert_eq!(
            strip_date_suffix("Team Sync, Apr 8 2026\n\n"),
            "Team Sync, Apr 8 2026\n\n"
        );
    }

    /// `\r\n` and a lone `\r` are line terminators to Java too, and both survive the
    /// replacement.
    #[test]
    fn carriage_returns_are_line_terminators_and_survive_the_strip() {
        assert_eq!(
            strip_date_suffix("Team Sync, Apr 8 2026\r\n"),
            "Team Sync\r\n"
        );
        assert_eq!(strip_date_suffix("Team Sync, Apr 8 2026\r"), "Team Sync\r");
    }

    /// The whole `dateSuffixRegex` shape, case by case, against the JVM's answers.
    /// Everything here is a way for a hand-rolled matcher to be too permissive.
    #[test]
    fn the_date_suffix_matcher_agrees_with_the_jvm_case_by_case() {
        let cases: [(&str, &str); 12] = [
            ("Team Sync, Apr 8 2026", "Team Sync"),
            ("Team Sync, Apr 08 2026", "Team Sync"),
            ("Team Sync, Apr 0 2026", "Team Sync"),
            (", Apr 8 2026", ""),
            ("A, Jan 1 2020, Feb 2 2021", "A, Jan 1 2020"),
            ("Team Sync, Apr 123 2026", "Team Sync, Apr 123 2026"),
            ("Team Sync, Apr 8 20267", "Team Sync, Apr 8 20267"),
            ("Team Sync, Apr 8 202", "Team Sync, Apr 8 202"),
            ("Team Sync Apr 8 2026", "Team Sync Apr 8 2026"),
            ("Team Sync,Apr 8 2026", "Team Sync,Apr 8 2026"),
            ("Team Sync, apr 8 2026", "Team Sync, apr 8 2026"),
            ("Team Sync, Foo 8 2026", "Team Sync, Foo 8 2026"),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_date_suffix(input), expected, "input: {input:?}");
        }
    }

    /// A date suffix that is not at the very end is not a suffix — `$` is an anchor,
    /// not a search.
    #[test]
    fn a_date_that_is_not_at_the_end_is_not_stripped() {
        assert_eq!(
            strip_date_suffix("Team Sync, Apr 8 2026 extra"),
            "Team Sync, Apr 8 2026 extra"
        );
    }

    /// Every month abbreviation in the alternation is accepted, and only those twelve.
    #[test]
    fn every_month_abbreviation_in_the_alternation_is_accepted() {
        for month in [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ] {
            assert_eq!(strip_date_suffix(&format!("Sync, {month} 1 2026")), "Sync");
        }
        for month in ["Sept", "Jly", "Mai", "JAN"] {
            let input = format!("Sync, {month} 1 2026");
            assert_eq!(strip_date_suffix(&input), input);
        }
    }

    /// C26.3, `TimeNormalizer.kt:31,129`. Each of the nine characters on its own, in a
    /// knowledge base built for that character alone — so a port that missed one has
    /// exactly one failing case rather than a lucky pass.
    #[test]
    fn every_filesystem_hostile_character_becomes_a_dash() {
        for hostile in HOSTILE_CHARS {
            let kb = knowledge_base(&["Calendar/A-B 2026-09-18.md"]);
            let entry = calendar_entry(&format!("A{hostile}B"));
            assert!(
                TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry),
                "{hostile:?} was not sanitized to a dash"
            );
        }
    }

    /// `replace(regex, "-")` is global, not first-only.
    #[test]
    fn every_occurrence_is_sanitized_not_only_the_first() {
        assert_eq!(sanitize("a:b:c"), "a-b-c");
        assert_eq!(sanitize(r#"a/b\c:d*e?f"g<h>i|j"#), "a-b-c-d-e-f-g-h-i-j");
    }

    /// A character that is legal in a filename is left alone — over-sanitizing would
    /// miss real calendar notes.
    #[test]
    fn characters_outside_the_hostile_set_are_left_alone() {
        assert_eq!(
            sanitize("AI Heads Sync (weekly) — #1 & 2, ты"),
            "AI Heads Sync (weekly) — #1 & 2, ты"
        );
    }

    /// C26.5, `TimeNormalizer.kt:23`. `Files.walk(root, 10)` reaches a directory whose
    /// path relative to the root has ten components — measured on the JVM, which finds
    /// `d1/…/d9/Calendar` and nothing deeper.
    #[test]
    fn a_calendar_directory_at_depth_ten_is_still_walked() {
        let kb =
            knowledge_base(&["d1/d2/d3/d4/d5/d6/d7/d8/d9/Calendar/AI Heads Sync 2026-09-18.md"]);
        let normalizer = TimeNormalizer::with_knowledge_base(kb.path());
        assert_eq!(normalizer.calendar_dirs().len(), 1);
        assert!(normalizer.is_meeting_entry(&calendar_entry("AI Heads Sync")));
    }

    /// C26.5, the other side of the same boundary: one level deeper is out of range,
    /// so the entry is scalable work.
    #[test]
    fn a_calendar_directory_at_depth_eleven_is_out_of_range() {
        let kb = knowledge_base(&[
            "d1/d2/d3/d4/d5/d6/d7/d8/d9/d10/Calendar/AI Heads Sync 2026-09-18.md",
        ]);
        let normalizer = TimeNormalizer::with_knowledge_base(kb.path());
        assert!(normalizer.calendar_dirs().is_empty());
        assert!(!normalizer.is_meeting_entry(&calendar_entry("AI Heads Sync")));
    }

    /// C26.5. The walk includes the start itself at depth 0.
    #[test]
    fn a_knowledge_base_root_named_calendar_is_itself_probed() {
        let outer = TempDir::new().expect("temp dir");
        let root = outer.path().join("Calendar");
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(root.join("AI Heads Sync 2026-09-18.md"), "").expect("write");
        let normalizer = TimeNormalizer::with_knowledge_base(&root);
        assert_eq!(normalizer.calendar_dirs(), std::slice::from_ref(&root));
        assert!(normalizer.is_meeting_entry(&calendar_entry("AI Heads Sync")));
    }

    /// The filter is `Files.isDirectory(it) && name == "Calendar"`; a plain file of
    /// that name is not a calendar directory.
    #[test]
    fn a_file_named_calendar_is_not_a_calendar_directory() {
        let kb = knowledge_base(&["Work/Calendar"]);
        assert!(
            TimeNormalizer::with_knowledge_base(kb.path())
                .calendar_dirs()
                .is_empty()
        );
    }

    /// The name comparison is exact, so a differently cased directory is not a hit.
    /// (On a case-insensitive filesystem the file inside it would still be reachable
    /// through a correctly cased path, which is why this asserts on the directory list
    /// rather than on `is_meeting_entry`.)
    #[test]
    fn a_calendar_directory_in_another_case_is_not_matched() {
        let kb = knowledge_base(&["Work/calendar/AI Heads Sync 2026-09-18.md"]);
        assert!(
            TimeNormalizer::with_knowledge_base(kb.path())
                .calendar_dirs()
                .is_empty()
        );
    }

    /// C26.5 — every calendar directory is probed, not just the first one found.
    #[test]
    fn every_calendar_directory_is_probed_not_only_the_first() {
        let kb = knowledge_base(&[
            "Work/Calendar/Something Else 2026-09-18.md",
            "Life/deep/Calendar/AI Heads Sync 2026-09-18.md",
        ]);
        let normalizer = TimeNormalizer::with_knowledge_base(kb.path());
        assert_eq!(normalizer.calendar_dirs().len(), 2);
        assert!(normalizer.is_meeting_entry(&calendar_entry("AI Heads Sync")));
    }

    /// `Files.walk` does not follow symlinks, but the `Files.isDirectory` in the
    /// filter does — so a symlink named `Calendar` pointing at a directory is a hit,
    /// while a symlink under another name is not descended into. Both measured.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_calendar_is_matched_but_never_descended_through() {
        let kb = knowledge_base(&["real/inner/keep.md", "elsewhere/Calendar/keep.md"]);
        fs::create_dir_all(kb.path().join("link")).expect("mkdir");
        std::os::unix::fs::symlink(kb.path().join("real"), kb.path().join("link/Calendar"))
            .expect("symlink");
        std::os::unix::fs::symlink(kb.path().join("elsewhere"), kb.path().join("hop"))
            .expect("symlink");

        let dirs = TimeNormalizer::with_knowledge_base(kb.path())
            .calendar_dirs()
            .to_vec();
        assert!(
            dirs.contains(&kb.path().join("link/Calendar")),
            "a symlink named Calendar should match: {dirs:?}"
        );
        assert!(
            !dirs.contains(&kb.path().join("hop/Calendar")),
            "the walk must not descend through a symlink: {dirs:?}"
        );
    }

    /// `TimeNormalizer.kt:26`. A knowledge base that cannot be walked is not an error;
    /// it makes every entry scalable work.
    #[test]
    fn a_knowledge_base_that_cannot_be_walked_yields_no_meetings() {
        let normalizer = offline();
        assert!(normalizer.calendar_dirs().is_empty());
        assert!(!normalizer.is_meeting_entry(&calendar_entry("AI Heads Sync")));
    }

    /// The date in the candidate filenames is `LocalDate.toString()`, i.e. ISO
    /// `YYYY-MM-DD` with zero padding — not `2026-9-8`.
    #[test]
    fn the_candidate_date_is_zero_padded_iso() {
        let kb = knowledge_base(&["Calendar/Team Sync 2026-04-08.md"]);
        let entry = DayProjectAggregate {
            date: date(2026, 4, 8),
            descriptions: vec!["Team Sync".to_string()],
            ..work("Delivery Practices", 0.5)
        };
        assert!(TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    /// The date is part of the probe, so yesterday's note does not make today a
    /// meeting — except through the third, dateless candidate, which is absent here.
    #[test]
    fn a_calendar_note_for_another_date_does_not_match() {
        let kb = knowledge_base(&["Calendar/AI Heads Sync 2026-09-17.md"]);
        assert!(
            !TimeNormalizer::with_knowledge_base(kb.path())
                .is_meeting_entry(&calendar_entry("AI Heads Sync"))
        );
    }

    /// Only the first description is used (`TimeNormalizer.kt:124`); a later one that
    /// would have matched is never tried.
    #[test]
    fn only_the_first_description_is_turned_into_a_filename() {
        let kb = knowledge_base(&["Calendar/Second Thing 2026-09-18.md"]);
        let entry = DayProjectAggregate {
            descriptions: vec!["First Thing".to_string(), "Second Thing".to_string()],
            ..work("Delivery Practices", 0.5)
        };
        assert!(!TimeNormalizer::with_knowledge_base(kb.path()).is_meeting_entry(&entry));
    }

    // -----------------------------------------------------------------------
    // C27 — quantization. `TimeNormalizer.kt:147-150`.
    // -----------------------------------------------------------------------

    /// C27: this helper is the **rounding** one. `0.375` is the value that separates
    /// it from the five truncating `toInt()` sites in `SettleCommand.kt`, which give
    /// `0.25` for the same input. A port that unified the two regimes dies here.
    #[test]
    fn round_to_quarter_rounds_a_half_increment_up_rather_than_truncating() {
        assert_eq!(round_to_quarter(0.375), 0.5);
        assert_eq!(round_to_quarter(0.124), 0.0);
        assert_eq!(round_to_quarter(0.125), 0.25);
        assert_eq!(round_to_quarter(7.9999), 8.0);
        assert_eq!(round_to_quarter(2.6666666666666665), 2.75);
    }

    /// `roundToInt()` sends halves toward `+∞`; `f64::round` sends them away from
    /// zero. They only disagree on negatives, and the second assertion pins the naive
    /// port's answer so the difference cannot be argued away.
    #[test]
    fn round_to_quarter_sends_negative_halves_toward_positive_infinity() {
        assert_eq!(round_to_quarter(-0.375), -0.25);
        assert_eq!((-0.375f64 / HOUR_INCREMENT).round() * HOUR_INCREMENT, -0.5);
        assert_eq!(round_to_quarter(-2.625), -2.5);
    }

    /// `Math.round` is exact round-half-up on the bit pattern, and
    /// `Math.round(0.49999999999999994) == 0` on the JVM while `floor(x + 0.5) == 1`.
    /// Second assertion pins what the `(x + 0.5).floor()` recipe would have produced.
    #[test]
    fn round_to_quarter_matches_the_jvm_where_floor_of_x_plus_a_half_does_not() {
        let hours = 0.49999999999999994 * HOUR_INCREMENT;
        assert_eq!(round_to_quarter(hours), 0.0);
        assert_eq!(
            ((hours / HOUR_INCREMENT) + 0.5).floor() * HOUR_INCREMENT,
            0.25
        );
    }

    /// The measured JVM table for `Math.round`, so the transcription is checked rather
    /// than trusted.
    #[test]
    fn kotlin_round_to_int_reproduces_the_measured_jvm_table() {
        let cases: [(f64, i32); 9] = [
            (0.49999999999999994, 0),
            (0.5, 1),
            (1.5, 2),
            (2.5, 3),
            (-2.5, -2),
            (-0.5, 0),
            (-1.5, -1),
            (2.0000000000000004, 2),
            (0.0, 0),
        ];
        for (input, expected) in cases {
            assert_eq!(kotlin_round_to_int(input), expected, "input: {input}");
        }
    }

    /// `roundToInt` saturates at the **32-bit** bounds, not the 64-bit ones. Wholly
    /// unreachable on real data; a three-character difference in the port.
    #[test]
    fn kotlin_round_to_int_saturates_at_the_32_bit_bounds() {
        assert_eq!(kotlin_round_to_int(1e30), i32::MAX);
        assert_eq!(kotlin_round_to_int(-1e30), i32::MIN);
        assert_eq!(kotlin_round_to_int(f64::INFINITY), i32::MAX);
        assert_eq!(round_to_quarter(1e30), f64::from(i32::MAX) * HOUR_INCREMENT);
    }

    /// `roundToInt` throws `IllegalArgumentException("Cannot round NaN value.")`.
    #[test]
    #[should_panic(expected = "Cannot round NaN value.")]
    fn round_to_quarter_rejects_nan_the_way_kotlin_does() {
        round_to_quarter(f64::NAN);
    }

    // -----------------------------------------------------------------------
    // C5 — 8h normalization. `TimeNormalizer.kt:40-109`.
    // -----------------------------------------------------------------------

    /// C5, `TimeNormalizer.kt:57-85`. The meeting keeps its hours and the work entry
    /// takes the whole adjustment.
    #[test]
    fn meetings_keep_their_hours_while_work_entries_are_scaled() {
        let result = offline().normalize(&[meeting("M", 2.0), work("W", 10.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("M".to_string(), 2.0), ("W".to_string(), 6.0),]
        );
    }

    /// C5, `TimeNormalizer.kt:57`. A `maxHours` cap makes an entry fixed even though
    /// it is not a meeting.
    #[test]
    fn a_capped_entry_is_fixed_alongside_the_meetings() {
        let result = offline().normalize(&[capped("C", 1.0, 1.0), work("W", 10.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("C".to_string(), 1.0), ("W".to_string(), 7.0),]
        );
    }

    /// C5. The cap marks the entry fixed but is never applied to its hours — the value
    /// carried forward is `totalHours`, not `maxHours`. Faithful to the incumbent, and
    /// the reason the cap is worth its own assertion.
    #[test]
    fn a_capped_entry_keeps_its_actual_hours_not_its_cap() {
        let result = offline().normalize(&[capped("C", 3.0, 1.0), work("W", 10.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("C".to_string(), 3.0), ("W".to_string(), 5.0),]
        );
    }

    /// C5, `TimeNormalizer.kt:66-68`. Already 8h: round and stop.
    #[test]
    fn a_day_of_exactly_eight_hours_passes_through_untouched() {
        let result = offline().normalize(&[work("A", 4.0), work("B", 4.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("A".to_string(), 4.0), ("B".to_string(), 4.0),]
        );
    }

    /// C5. A day a ten-thousandth short of 8h is inside the shortcut and rounds to 8h.
    #[test]
    fn a_day_of_seven_point_nine_nine_nine_nine_hours_rounds_to_eight() {
        let result = offline().normalize(&[work("A", 7.9999)]);
        assert_eq!(hours_by_project(&result), vec![("A".to_string(), 8.0)]);
    }

    /// C5, `TimeNormalizer.kt:66`. The shortcut skips the 0.25h floor entirely, so a
    /// zero-hour entry stays at zero instead of being floored — the only observable
    /// difference between the two paths on this input.
    #[test]
    fn the_already_eight_shortcut_skips_the_quarter_hour_floor() {
        let result = offline().normalize(&[work("A", 8.1), work("B", 0.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("A".to_string(), 8.0), ("B".to_string(), 0.0),]
        );
    }

    /// C5, `TimeNormalizer.kt:66`. The comparison is a strict `<` against half an
    /// increment, so 8.125 falls through to scaling. Under a `<=` the shortcut would
    /// fire and round 8.125 up to 8.25 — which is what this asserts against.
    #[test]
    fn a_day_exactly_half_an_increment_over_eight_is_scaled_not_shortcut() {
        let result = offline().normalize(&[work("A", 8.125)]);
        assert_eq!(hours_by_project(&result), vec![("A".to_string(), 8.0)]);
        assert_eq!(
            round_to_quarter(8.125),
            8.25,
            "the shortcut would have given this"
        );
    }

    /// C5, `TimeNormalizer.kt:74-76`. No scalable entries: round and stop, even though
    /// the day is nowhere near 8h.
    #[test]
    fn a_meeting_only_day_is_only_rounded_never_scaled() {
        let result = offline().normalize(&[meeting("A", 3.1), meeting("B", 2.2)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("A".to_string(), 3.0), ("B".to_string(), 2.25),]
        );
    }

    /// C5. A meeting-only day over 8h stays over 8h — nothing scales meetings down.
    #[test]
    fn a_meeting_only_day_above_eight_hours_is_left_above_eight() {
        let result = offline().normalize(&[meeting("A", 5.0), meeting("B", 5.0)]);
        assert_eq!(sum_hours(&result), 10.0);
    }

    /// C5, `TimeNormalizer.kt:74`. Meetings already past 8h make the work target
    /// negative, so the work entries are merely rounded, not scaled to a negative.
    #[test]
    fn a_negative_work_target_leaves_the_work_entries_only_rounded() {
        let result = offline().normalize(&[meeting("M", 9.0), work("W", 1.1)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("M".to_string(), 9.0), ("W".to_string(), 1.0),]
        );
    }

    /// C5, `TimeNormalizer.kt:74`. The guard is `<= 0`, so a target of exactly zero
    /// takes the same exit — with `< 0` the scale factor would be 0 and the floor
    /// would put every work entry at 0.25.
    #[test]
    fn a_work_target_of_exactly_zero_leaves_the_work_entries_only_rounded() {
        let result = offline().normalize(&[meeting("M", 8.0), work("W", 1.1)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("M".to_string(), 8.0), ("W".to_string(), 1.0),]
        );
    }

    /// C5, `TimeNormalizer.kt:82-85`. A near-zero entry is floored at one increment
    /// rather than rounded away to nothing.
    #[test]
    fn a_tiny_work_entry_is_floored_at_a_quarter_hour_rather_than_vanishing() {
        let result = offline().normalize(&[work("A", 0.05), work("B", 10.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("A".to_string(), 0.25), ("B".to_string(), 7.75),]
        );
    }

    /// C5, `TimeNormalizer.kt:97-102`. Three equal entries round to 8.25 between them,
    /// and the whole −0.25 residual goes to one of them.
    #[test]
    fn the_rounding_residual_lands_on_a_single_entry_and_the_day_hits_eight() {
        let result = offline().normalize(&[work("A", 1.0), work("B", 1.0), work("C", 1.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![
                ("A".to_string(), 2.5),
                ("B".to_string(), 2.75),
                ("C".to_string(), 2.75),
            ]
        );
        assert_eq!(sum_hours(&result), 8.0);
    }

    /// C28, `TimeNormalizer.kt:99-100`. `sortedByDescending {}.first()` is stable, so
    /// the residual goes to the **first** of two equally large entries. Rust's
    /// `max_by`/`max_by_key` return the last maximum and would put 3.75 on B — moving
    /// a quarter hour between two projects on an ordinary day.
    #[test]
    fn the_residual_lands_on_the_first_of_two_equally_large_entries() {
        let result = offline().normalize(&[meeting("M", 0.25), work("A", 1.0), work("B", 1.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![
                ("A".to_string(), 3.75),
                ("B".to_string(), 4.0),
                ("M".to_string(), 0.25),
            ]
        );
        assert_eq!(sum_hours(&result), 8.0);
    }

    /// C28, the same site with the tie broken the other way round: reversing the input
    /// moves the residual with it, which is what makes the previous test a statement
    /// about order rather than about project names.
    #[test]
    fn reversing_two_equally_large_entries_moves_the_residual_with_them() {
        let result = offline().normalize(&[meeting("M", 0.25), work("B", 1.0), work("A", 1.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![
                ("A".to_string(), 4.0),
                ("B".to_string(), 3.75),
                ("M".to_string(), 0.25),
            ]
        );
    }

    /// C5, `TimeNormalizer.kt:99-101`. With no tie, the residual goes to the strictly
    /// largest scaled entry — here C, not the two small ones.
    #[test]
    fn the_residual_lands_on_the_strictly_largest_scaled_entry() {
        let result = offline().normalize(&[work("A", 1.0), work("B", 1.0), work("C", 5.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![
                ("A".to_string(), 1.25),
                ("B".to_string(), 1.25),
                ("C".to_string(), 5.5),
            ]
        );
        assert_eq!(sum_hours(&result), 8.0);
    }

    /// C5, `TimeNormalizer.kt:97`. The residual is applied only when it reaches half
    /// an increment; a day that already lands on target is left alone.
    #[test]
    fn a_day_that_already_lands_on_target_gets_no_residual_adjustment() {
        let result = offline().normalize(&[meeting("M", 2.0), work("A", 1.0), work("B", 2.0)]);
        assert_eq!(
            hours_by_project(&result),
            vec![
                ("A".to_string(), 2.0),
                ("B".to_string(), 4.0),
                ("M".to_string(), 2.0),
            ]
        );
    }

    /// C5, `TimeNormalizer.kt:101`. The 0.25h floor applies to the adjusted entry too,
    /// and it wins over the residual — so this day ends on 8.25h, not 8h. The
    /// incumbent's guarantee is "adjust the largest entry", not "always reach 8h".
    #[test]
    fn the_quarter_hour_floor_beats_the_residual_and_the_day_can_overshoot_eight() {
        let result = offline().normalize(&[meeting("M", 7.9), work("W", 0.5)]);
        assert_eq!(
            hours_by_project(&result),
            vec![("M".to_string(), 8.0), ("W".to_string(), 0.25),]
        );
        assert_eq!(sum_hours(&result), 8.25);
    }

    /// C5. A single work entry is scaled straight onto the target.
    #[test]
    fn a_day_of_one_work_entry_is_scaled_to_exactly_eight() {
        let result = offline().normalize(&[work("A", 6.4)]);
        assert_eq!(hours_by_project(&result), vec![("A".to_string(), 8.0)]);
    }

    /// C5. A day that is one long meeting and nothing else is not touched at all.
    #[test]
    fn a_single_meeting_day_is_neither_scaled_nor_floored() {
        let result = offline().normalize(&[meeting("M", 1.5)]);
        assert_eq!(hours_by_project(&result), vec![("M".to_string(), 1.5)]);
    }

    /// C5. `workHours` is zero while `workEntries` is not empty, so the scale factor is
    /// infinite and `0.0 * ∞` is NaN — which `roundToInt` turns into an
    /// `IllegalArgumentException` in the incumbent. Reproduced rather than repaired:
    /// the parity bar covers the crashes too.
    #[test]
    #[should_panic(expected = "Cannot round NaN value.")]
    fn a_day_whose_only_work_entry_has_zero_hours_dies_the_way_the_incumbent_does() {
        offline().normalize(&[meeting("M", 2.0), work("W", 0.0)]);
    }

    // -----------------------------------------------------------------------
    // C28 — ordering. `TimeNormalizer.kt:42,46,99,108`.
    // -----------------------------------------------------------------------

    /// C5, `TimeNormalizer.kt:42-44`. Days are normalized independently; the 12h day
    /// is scaled to 8h without borrowing anything from the 4h one.
    #[test]
    fn each_day_is_normalized_independently_of_the_others() {
        let mut second = work("A", 4.0);
        second.date = date(2026, 9, 19);
        let result = offline().normalize(&[work("A", 12.0), second]);
        assert_eq!(result[0].original.date, date(2026, 9, 18));
        assert_eq!(result[0].normalized_hours, 8.0);
        assert_eq!(result[1].original.date, date(2026, 9, 19));
        assert_eq!(result[1].normalized_hours, 8.0);
    }

    /// C28, `TimeNormalizer.kt:46`. Output order is `(date, devproProjectName)`, not
    /// input order.
    #[test]
    fn the_output_is_ordered_by_date_then_project_name() {
        let mut earlier = work("Zeta", 4.0);
        earlier.date = date(2026, 9, 17);
        let result = offline().normalize(&[work("Beta", 4.0), work("Alpha", 4.0), earlier]);
        let order: Vec<_> = result
            .iter()
            .map(|e| (e.original.date, e.original.devpro_project_name.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                (date(2026, 9, 17), "Zeta"),
                (date(2026, 9, 18), "Alpha"),
                (date(2026, 9, 18), "Beta"),
            ]
        );
    }

    /// C28, `TimeNormalizer.kt:107`. On the scaling path the concatenation is
    /// `roundedFixed + finalWork`, so a fixed row precedes a scaled row that shares its
    /// `(date, project)` key — whatever order they arrived in.
    #[test]
    fn on_the_scaling_path_a_fixed_row_precedes_a_tied_scaled_row() {
        let result = offline().normalize(&[work("P", 9.0), meeting("P", 1.0)]);
        assert!(result[0].is_meeting, "the meeting should lead the tie");
        assert_eq!(result[0].normalized_hours, 1.0);
        assert_eq!(result[1].normalized_hours, 7.0);
    }

    /// C28, `TimeNormalizer.kt:67`. The already-8h shortcut never builds that
    /// concatenation, so the same tie keeps the input order instead. Two paths, two
    /// answers for identical rows — worth pinning, because a port that hoisted the
    /// sort out of the branches would quietly unify them.
    #[test]
    fn on_the_shortcut_path_a_tied_row_keeps_its_input_order() {
        let result = offline().normalize(&[work("P", 7.0), meeting("P", 1.0)]);
        assert!(
            !result[0].is_meeting,
            "the work entry arrived first and stays first"
        );
        assert_eq!(result[0].normalized_hours, 7.0);
        assert!(result[1].is_meeting);
    }

    /// C28's cheap guard: a randomized map anywhere in the pipeline makes consecutive
    /// runs disagree. Six rows sharing a `(date, project)` key is the shape the
    /// captured 2026-09-18 run actually has.
    #[test]
    fn normalizing_the_same_input_twice_produces_the_same_output() {
        let input: Vec<_> = (0..6).map(|_| work("Inveniam SOW #5", 1.5)).collect();
        let normalizer = offline();
        assert_eq!(normalizer.normalize(&input), normalizer.normalize(&input));
    }

    /// The degenerate input, which must not panic on the empty `sorted.first()`.
    #[test]
    fn an_empty_input_yields_an_empty_output() {
        assert!(offline().normalize(&[]).is_empty());
    }

    /// The `isMeeting` flag computed at `TimeNormalizer.kt:52` survives into the
    /// result — `settle.rs` reads it to decide filler and borrowing eligibility.
    #[test]
    fn the_meeting_flag_is_carried_through_to_the_result() {
        let result = offline().normalize(&[meeting("M", 2.0), work("W", 10.0)]);
        assert!(result[0].is_meeting);
        assert!(!result[1].is_meeting);
        assert_eq!(
            result[0].original.chrono_project,
            "Operations - DevPro - Work"
        );
    }
}
