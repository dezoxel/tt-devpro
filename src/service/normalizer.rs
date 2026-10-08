//! Meeting detection, ported from `service/TimeNormalizer.kt` (C26). The 8-hour scaling the
//! Kotlin object also carried is gone: the plan model spreads the hours now, and a meeting is
//! the one thing it is never allowed to stretch — which is what this probe decides.
//!
//! One detail is a literal translation of JVM semantics rather than idiomatic Rust, measured
//! against GraalVM JDK 21 before being written down: **Java's `$` matches before a trailing
//! line terminator, and `replaceAll` keeps that terminator.** `"Event, Apr 8 2026\n"` becomes
//! `"Event\n"` on the JVM, not `"Event"`. A Rust regex anchored `(?:\r?\n)?$` with an empty
//! replacement would eat the newline and derive a different meeting filename, so the
//! date-suffix strip is hand-rolled to keep the terminator.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::model::DayProjectAggregate;

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
/// computed once per process from the constant `~/knowledge-base`. Here the root is
/// a parameter: `settle` takes it from `vault_path` in `~/.config/tt-devpro/config.yaml`, because
/// the vault sits at a different path on each machine, and the tests hand in a
/// temporary directory, which is what makes C26 testable.
pub struct TimeNormalizer {
    calendar_dirs: Vec<PathBuf>,
}

impl TimeNormalizer {
    /// The normalizer `settle` runs with: the root from the config, and a walk that
    /// fails loudly instead of yielding "no meetings".
    ///
    /// **A deliberate divergence from C26**, made for the same reason as D3. The
    /// incumbent turns a missing or unreadable knowledge base into an empty list, so
    /// every meeting is scaled as work and posted with the wrong hours, and nothing
    /// says so. Here the root must exist and be a directory, an I/O error partway
    /// through the walk stops the run with the path that failed, and a walk that finds
    /// no `Calendar` directory at all is an error too — that catches a root that
    /// exists but is the wrong directory. The unit tests below cover it; the parity
    /// harness cannot, because the incumbent never fails here.
    ///
    /// The root is canonicalized first, so a symlink to the vault is followed: the
    /// walk itself never descends through a symlink.
    pub fn for_settle(root: &Path) -> Result<Self> {
        let resolved = std::fs::canonicalize(root).with_context(|| {
            format!(
                "{VAULT_PATH_KEY} points at {}, which cannot be resolved",
                root.display()
            )
        })?;
        if !resolved.is_dir() {
            bail!(
                "{VAULT_PATH_KEY} points at {}, which is not a directory",
                resolved.display()
            );
        }
        let calendar_dirs = walk_for_calendar_dirs(&resolved)
            .with_context(|| format!("walking the knowledge base at {}", resolved.display()))?;
        if calendar_dirs.is_empty() {
            bail!(
                "{VAULT_PATH_KEY} points at {}, which has no Calendar folder, so no \
                 meeting could be detected. Is it the vault?",
                resolved.display()
            );
        }
        Ok(Self { calendar_dirs })
    }

    /// The incumbent's lenient walk (C26): a root that is missing or cannot be read
    /// yields no meetings. Test-only — the C26 cases pin it, and the settle tests
    /// build fixtures with it; `settle` itself goes through
    /// [`TimeNormalizer::for_settle`].
    #[cfg(test)]
    pub fn with_knowledge_base(root: impl AsRef<Path>) -> Self {
        Self {
            calendar_dirs: find_calendar_dirs(root.as_ref()),
        }
    }

    /// Every directory named `Calendar` the walk found, in walk order.
    ///
    /// Test-only, matching the incumbent, whose `calendarDirs` is `private` and
    /// read at `TimeNormalizer.kt:136` alone. [`TimeNormalizer::is_meeting_entry`]
    /// reads the field directly, so exposing it buys production nothing; C26 — which
    /// directories the depth-10 walk finds — is only checkable through it.
    #[cfg(test)]
    pub fn calendar_dirs(&self) -> &[PathBuf] {
        &self.calendar_dirs
    }

    /// `TimeNormalizer.isMeetingEntry` (`TimeNormalizer.kt:111-145`) — C26.
    pub fn is_meeting_entry(&self, agg: &DayProjectAggregate) -> bool {
        // `TimeNormalizer.kt:113-115` — admin work is a meeting unconditionally, before any I/O.
        if agg.chrono_project.starts_with("Operations -") {
            return true;
        }

        // `TimeNormalizer.kt:117-119`.
        let Some(raw_description) = agg.descriptions.first() else {
            return false;
        };

        // `TimeNormalizer.kt:123-129`.
        let project_suffix = format!(" - {}", agg.chrono_project);
        let meeting_name = strip_date_suffix(remove_suffix(raw_description, &project_suffix));
        let sanitized = sanitize(&meeting_name);

        // `TimeNormalizer.kt:130-134` — the middle candidate's double space is
        // load-bearing: it catches a trailing space in the upstream `display_name`.
        // Deleting it reclassifies real meetings as scalable work.
        let date = agg.date.to_string();
        let candidates = [
            format!("{sanitized} {date}.md"),
            format!("{sanitized}  {date}.md"),
            format!("{sanitized}.md"),
        ];

        // `TimeNormalizer.kt:136-144`.
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

/// `kotlin.math.min(Double, Double)` is `Math.min`, which `f64::min` disagrees with twice,
/// both measured on GraalVM JDK 21.0.11 (`~/.cache/tt-devpro-rewrite/measurements/maxcmp/`):
/// `f64::min` discards NaN and hands back the other operand, and it is documented to return
/// either operand when both are zero. The Jaro-Winkler cap in [`crate::commands`] is the
/// caller.
///
/// The signed-zero test is `b.is_sign_negative()` where the JDK compares raw bits; inside the
/// `a == 0.0 && b == 0.0` guard the two are the same predicate.
pub fn java_min(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && b.is_sign_negative() {
        return b;
    }
    if a <= b { a } else { b }
}

/// Names the setting in every error [`TimeNormalizer::for_settle`] raises, so the
/// operator knows what to fix.
const VAULT_PATH_KEY: &str = "vault_path in ~/.config/tt-devpro/config.yaml";

/// `TimeNormalizer.kt:21-29`. A missing or unreadable root, or an unreadable
/// subdirectory partway through, yields an empty list, never an error. Only the
/// tests build a normalizer this way; `settle` goes through
/// [`TimeNormalizer::for_settle`].
#[cfg(test)]
fn find_calendar_dirs(root: &Path) -> Vec<PathBuf> {
    walk_for_calendar_dirs(root).unwrap_or_default()
}

fn walk_for_calendar_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    // `Files.walk` reads the start's attributes first and throws if it cannot.
    let start = std::fs::symlink_metadata(root).with_context(|| root.display().to_string())?;
    let mut out = Vec::new();
    collect_calendar_dirs(root, start.is_dir(), 0, &mut out)?;
    Ok(out)
}

/// `Files.walk` without `FOLLOW_LINKS` does not descend through a symlink, while the
/// `Files.isDirectory` in the filter *does* follow it — so a symlink named `Calendar`
/// that points at a directory matches but is not walked into. Both halves measured.
///
/// An entry that vanishes between the listing and the read (`NotFound`) is skipped,
/// and the walk goes on. The vault is written by background syncs, so that race is
/// ordinary. This is the second deliberate divergence next to C26: the incumbent
/// gives up on the whole walk there. Any other error carries the path that failed.
fn collect_calendar_dirs(
    path: &Path,
    descendable: bool,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    if path.file_name().is_some_and(|name| name == "Calendar") && path.is_dir() {
        out.push(path.to_path_buf());
    }
    if !descendable || depth == MAX_WALK_DEPTH {
        return Ok(());
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| path.display().to_string()),
    };
    for entry in entries {
        let entry = entry.with_context(|| path.display().to_string())?;
        let child_is_real_dir = match entry.file_type() {
            Ok(file_type) => file_type.is_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| entry.path().display().to_string());
            }
        };
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
///
/// **The single copy.** The incumbent carries this regex three times —
/// `TimeNormalizer.kt:32` (C26, the meeting-filename probe), `SettleCommand.kt:486`
/// and `BorrowerService.kt:145` (both C12, the task title). They are one pattern, so
/// they are one function here, and the plan context's title cleaning calls this one.
///
/// It does, measured 2026-09-22 over **3 338 100** adversarial inputs — the
/// deduped product of five prefixes, five separators, eighteen month tokens
/// (`jan`, `JAN`, `Jann`, `Ja`, `Sept` among them), three day separators, twelve
/// day tokens, three year separators, eight year tokens and nine trailing
/// terminators, of which **8 820** match and the rest are near misses that must
/// not be stripped. This function's output and `java.util.regex`'s under the
/// incumbent's own pattern hash to the same sha256 `fb0e9375…` and `diff` exits
/// 0. Generator, both probes and the commands:
/// `~/.cache/tt-devpro-rewrite/measurements/datecmp/README.md`.
///
/// `pub` because the plan context strips the same suffix from a meeting's title, and a
/// second copy of a measured matcher is how two of them start to drift.
pub fn strip_date_suffix(s: &str) -> String {
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

    /// An entry that is no meeting: not an `Operations -` project, and a description that
    /// will not match anything on disk.
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

    /// No knowledge base on disk, so `calendarDirs` is empty and only the
    /// `Operations -` short circuit can produce a meeting.
    fn offline() -> TimeNormalizer {
        TimeNormalizer::with_knowledge_base("/definitely/not/a/knowledge/base")
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
            chrono_project: "Operations - DevPro - Work".to_string(),
            descriptions: vec![],
            ..work("Operations", 1.0)
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

    /// The two strips fail **together** on a trailing line terminator, and the
    /// entry silently stops being a meeting. C12/C26, measured on JDK 21 running
    /// the whole incumbent pipeline, re-run 2026-09-22
    /// (`~/.cache/tt-devpro-rewrite/measurements/suffix/Suffix.java`):
    ///
    /// ```text
    /// <Team Sync, Apr 8 2026 - Practices - DevPro - Work>    -> <Team Sync>
    /// <Team Sync, Apr 8 2026 - Practices - DevPro - Work\n>  -> unchanged
    /// <Team Sync, Apr 8 2026\n>                              -> <Team Sync\n>
    /// ```
    ///
    /// `removeSuffix` is an exact end-of-string test, so the terminator makes it
    /// miss; the surviving project suffix then pushes the date out of `$`'s reach,
    /// so the date is not stripped either. The third line is the control: with no
    /// project suffix in the way, the date still goes and the terminator still
    /// survives, which is what says the failure belongs to the composition rather
    /// than to either half.
    ///
    /// Rust's `strip_suffix` reproduces this by construction. The test exists
    /// because the repair anyone would reach for — trimming the description before
    /// stripping — is invisible in a diff and turns a scalable-work day into a
    /// meeting day, changing hours on real dates. Under that repair the first
    /// assertion below flips to `true`.
    #[test]
    fn a_trailing_newline_defeats_both_strips_and_the_meeting_is_missed() {
        let kb = knowledge_base(&["Calendar/Team Sync 2026-04-08.md"]);
        let normalizer = TimeNormalizer::with_knowledge_base(kb.path());

        let with_terminator = DayProjectAggregate {
            date: date(2026, 4, 8),
            chrono_project: "Practices - DevPro - Work".to_string(),
            descriptions: vec!["Team Sync, Apr 8 2026 - Practices - DevPro - Work\n".to_string()],
            ..work("Delivery Practices", 0.5)
        };
        assert!(
            !normalizer.is_meeting_entry(&with_terminator),
            "neither strip fires, so the probe looks for the whole raw description as a filename and finds nothing"
        );

        // The premise: the identical entry without the terminator IS a meeting, so
        // the assertion above is about the terminator and not about the fixture.
        let without_terminator = DayProjectAggregate {
            descriptions: vec!["Team Sync, Apr 8 2026 - Practices - DevPro - Work".to_string()],
            ..with_terminator.clone()
        };
        assert!(normalizer.is_meeting_entry(&without_terminator));

        // The control: with no project suffix to survive, the date strip works and
        // the terminator is carried into the probed name.
        assert_eq!(strip_date_suffix("Team Sync, Apr 8 2026\n"), "Team Sync\n");
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

    /// `TimeNormalizer.kt:26`. A knowledge base that cannot be walked is not an error
    /// here; it makes every entry scalable work. `settle` does not go through this
    /// path: [`TimeNormalizer::for_settle`] fails instead, as the tests below show.
    #[test]
    fn a_knowledge_base_that_cannot_be_walked_yields_no_meetings() {
        let normalizer = offline();
        assert!(normalizer.calendar_dirs().is_empty());
        assert!(!normalizer.is_meeting_entry(&calendar_entry("AI Heads Sync")));
    }

    /// `for_settle` on a root that does not exist names the root and the key.
    #[test]
    fn for_settle_fails_on_a_missing_root() {
        let dir = TempDir::new().expect("temp dir");
        let missing = dir.path().join("no-vault-here");
        let message = format!(
            "{:#}",
            TimeNormalizer::for_settle(&missing)
                .err()
                .expect("must fail")
        );
        assert!(
            message.contains("vault_path in ~/.config/tt-devpro/config.yaml"),
            "{message}"
        );
        assert!(message.contains("no-vault-here"), "{message}");
    }

    #[test]
    fn for_settle_fails_on_a_file_instead_of_a_directory() {
        let kb = knowledge_base(&["vault.md"]);
        let message = format!(
            "{:#}",
            TimeNormalizer::for_settle(&kb.path().join("vault.md"))
                .err()
                .expect("must fail")
        );
        assert!(message.contains("is not a directory"), "{message}");
    }

    /// The case the incumbent cannot see: the root exists, but it is not the vault.
    #[test]
    fn for_settle_fails_on_a_directory_without_any_calendar() {
        let kb = knowledge_base(&["Work/Notes/plain.md"]);
        let message = format!(
            "{:#}",
            TimeNormalizer::for_settle(kb.path())
                .err()
                .expect("must fail")
        );
        assert!(message.contains("has no Calendar folder"), "{message}");
    }

    #[test]
    fn for_settle_finds_meetings_in_a_real_vault() {
        let kb = knowledge_base(&["Work/Calendar/AI Heads Sync 2026-09-18.md"]);
        let normalizer = TimeNormalizer::for_settle(kb.path()).expect("a vault with a Calendar");
        assert!(normalizer.is_meeting_entry(&calendar_entry("AI Heads Sync")));
    }

    /// The vault may be reached through a symlink. The walk never descends through
    /// one, so `for_settle` resolves the root before walking.
    #[cfg(unix)]
    #[test]
    fn for_settle_follows_a_symlinked_root() {
        let kb = knowledge_base(&["vault/Work/Calendar/AI Heads Sync 2026-09-18.md"]);
        let link = kb.path().join("link-to-vault");
        std::os::unix::fs::symlink(kb.path().join("vault"), &link).expect("symlink");
        let normalizer = TimeNormalizer::for_settle(&link).expect("the symlink is followed");
        assert!(normalizer.is_meeting_entry(&calendar_entry("AI Heads Sync")));
    }

    /// Restores a directory's permissions on drop, before its `TempDir` is removed.
    #[cfg(unix)]
    struct RestoreMode(PathBuf);

    #[cfg(unix)]
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    /// An unreadable folder partway through the walk stops `settle` with that
    /// folder's path, rather than turning into "no Calendar folder" and pointing the
    /// operator at the config.
    #[cfg(unix)]
    #[test]
    fn for_settle_reports_an_unreadable_folder_by_its_path() {
        use std::os::unix::fs::PermissionsExt;
        let kb = knowledge_base(&["Work/Calendar/AI Heads Sync 2026-09-18.md", "Locked/x.md"]);
        let locked = kb.path().join("Locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");
        let _restore = RestoreMode(locked.clone());
        assert!(
            fs::read_dir(&locked).is_err(),
            "a folder with mode 000 was readable: this test needs to run as a non-root user"
        );

        let message = format!(
            "{:#}",
            TimeNormalizer::for_settle(kb.path())
                .err()
                .expect("must fail")
        );
        assert!(message.contains("Locked"), "{message}");
        assert!(!message.contains("has no Calendar folder"), "{message}");
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
    // The JDK float primitive this module owns
    // -----------------------------------------------------------------------

    /// `Math.min`'s two departures from `f64::min`, read off GraalVM JDK 21.0.11.
    ///
    /// The `f64::min` assertions are the point — they pin the divergence rather than
    /// the agreement, so a port that "simplifies" `java_min` back to `a.min(b)` fails
    /// here instead of in production.
    #[test]
    fn java_min_propagates_nan_and_orders_the_zeroes_where_f64_min_does_neither() {
        assert!(java_min(f64::NAN, 1.0).is_nan());
        assert!(java_min(1.0, f64::NAN).is_nan());
        assert_eq!(f64::NAN.min(1.0), 1.0);
        assert_eq!(1.0_f64.min(f64::NAN), 1.0);

        assert!(java_min(0.0, -0.0).is_sign_negative());
        assert!(java_min(-0.0, 0.0).is_sign_negative());
        assert!(java_min(-0.0, -0.0).is_sign_negative());
        assert!(java_min(0.0, 0.0).is_sign_positive());

        assert_eq!(java_min(1.0, 2.0), 1.0);
        assert_eq!(java_min(2.0, 1.0), 1.0);
        assert_eq!(java_min(-3.0, 2.0), -3.0);
        assert_eq!(java_min(0.25, 0.0), 0.0);
    }
}
