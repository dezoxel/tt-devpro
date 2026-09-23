//! The comment-width gate.
//!
//! `cargo fmt` holds code to 100 columns and leaves comments alone: `wrap_comments` is a
//! nightly-only option and is off by default. A comment can therefore be any width at all
//! while every gate in the suite stays green — `cargo test`, `cargo clippy`, `cargo fmt
//! --check` and `cargo doc` all read over it without a word.
//!
//! That is not hypothetical, and it is not fixed by being careful once. Measured with the
//! rule below, the rounds that gave the Kotlin citations their file names — turning `:NNN`
//! into `SettleCommand.kt:NNN` — took the tree from 5 over-wide comment lines to 67, every
//! gate green throughout. A wrap pass (`84dcb03`) then brought it to 49 and stopped there,
//! and 49 is what stood until the rewrap this gate arrived with. Point the rule at any of
//! those commits and it fails: that is where the evidence it *can* fail lives, together
//! with the three tests under it, since a green run says something only while the rule
//! still recognises a wide line. The first count of the 49 was itself wrong: `awk
//! 'length>100'` on macOS counts bytes and not characters, so the em-dashes these comments
//! are full of inflated 49 into 58. This gate counts characters, and pins that.
//!
//! Scope is every `*.rs` under `src/` and `tests/`, this file included. Code lines are not
//! checked: rustfmt already owns them, and a long string literal it cannot break is not a
//! defect anyone can fix.

use std::fs;
use std::path::{Path, PathBuf};

/// The widest a comment line may be, counted in characters.
///
/// The number is rustfmt's own `max_width`, which this project takes at its default. Code
/// is held there by the formatter; comments are held there by hand and by this test,
/// because the option that would wrap them — `wrap_comments` — is nightly-only and off by
/// default, so no stable toolchain will do it.
///
/// It is also where the tree already sat. Before the citation rounds, at `4ee5ce7`, 137 of
/// its 6092 comment lines were 87 to 99 characters wide and exactly 5 were over 100. The
/// 80s and 90s are lived-in, in other words, and the limit sits just past them: lowering it
/// would not tighten a habit, it would declare 137 lines defective that nobody wrote as
/// defects.
const MAX_WIDTH: usize = 100;

fn repo_root() -> PathBuf {
    // Not the current directory: `cargo test` may run the binary from anywhere, while
    // the manifest directory is the repository by construction.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A line is a comment line when what follows its indent is `//`.
///
/// That covers `//`, `///` and `//!` in one rule, and deliberately covers nothing else: a
/// trailing comment after code is measured as the code line it shares, and the code line
/// is rustfmt's business.
fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// The width of a line in characters.
///
/// `str::len` would answer in bytes, and that is exactly the mistake this gate was written
/// after: on a comment carrying `—` or `→`, bytes overcount, and the overcount is a
/// violation reported against a line that is already within the limit.
fn width(line: &str) -> usize {
    line.chars().count()
}

/// The whole rule, in one place so the tests below can put lines through it directly.
fn is_too_wide(line: &str) -> bool {
    is_comment(line) && width(line) > MAX_WIDTH
}

/// Every `*.rs` under `src/` and `tests/`, sorted, so a failing run lists its findings in
/// the same order every time and a fixer can work straight down the list.
fn rust_sources() -> Vec<PathBuf> {
    let root = repo_root();
    let mut out = Vec::new();
    for dir in ["src", "tests"] {
        collect_rust(&root.join(dir), &mut out);
    }
    assert!(
        !out.is_empty(),
        "comment-width gate: found no .rs file under src/ or tests/ — the scan would pass vacuously."
    );
    out.sort();
    out
}

fn collect_rust(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("comment-width gate: cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("comment-width gate: cannot read {}: {e}", dir.display()))
            .path();
        if path.is_dir() {
            collect_rust(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn relative(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Reports every violation at once, one per line, and prints `remedy` once underneath
/// rather than repeating it on each line.
///
/// Failing on the first violation would turn a fix into one `cargo test` run per defect,
/// and the round that prompted this gate left 49 at a time.
fn report(header: &str, violations: &[String], remedy: &str) {
    if violations.is_empty() {
        return;
    }
    // The count trails the header rather than leading it, so one violation does not
    // read as "1 lines".
    let mut message = format!(
        "{header} ({}):\n{}",
        violations.len(),
        violations.join("\n")
    );
    if !remedy.is_empty() {
        message.push_str("\n\n");
        message.push_str(remedy);
    }
    panic!("{message}\n");
}

/// Catches a comment that runs past the width the code beside it is held to.
///
/// Without this test the width of a comment is checked by nobody. rustfmt reformats the
/// code around it and leaves the comment exactly as written, so an edit that appends to a
/// comment — a file name added to a citation, a clause added to a sentence — can push it
/// to any length and still pass every gate the project runs.
#[test]
fn no_comment_line_is_wider_than_the_limit() {
    let mut violations = Vec::new();
    let mut scanned = 0usize;

    for source in rust_sources() {
        let body = fs::read_to_string(&source).unwrap_or_else(|e| {
            panic!("comment-width gate: cannot read {}: {e}", source.display())
        });
        let rust = relative(&source);

        for (index, line) in body.lines().enumerate() {
            scanned += 1;
            if is_too_wide(line) {
                violations.push(format!(
                    "{rust}:{}: {} chars — {}",
                    index + 1,
                    width(line),
                    line.trim()
                ));
            }
        }
    }

    // A walk that found files but read nothing out of them would report zero violations
    // and look exactly like a clean tree.
    assert!(
        scanned > 0,
        "comment-width gate: the .rs files under src/ and tests/ hold no lines at all."
    );

    report(
        &format!("comment lines wider than {MAX_WIDTH} characters"),
        &violations,
        "Rewrap the comment by hand at the reported line. `cargo fmt` will not do it: rustfmt's `wrap_comments` is nightly-only and off by default, which is the whole reason this test exists. Measure with `line.chars().count()`, not with `awk 'length>100'` — macOS awk counts bytes, and a comment full of em-dashes is reported wide while fitting.",
    );
}

/// Pins that the rule reads characters and not bytes.
///
/// This is the measurement mistake itself, written down. The first count of the violations
/// this gate was built for used `awk 'length>100'`, whose `length` on macOS is a byte
/// count, and it reported 58 violations where there were 49 — nine lines that fit, blamed
/// for the width of their own em-dashes. A gate that repeated that mistake would send its
/// reader to rewrap lines that are already within the limit.
#[test]
fn the_rule_counts_characters_and_not_bytes() {
    let ascii = format!("// {}", "x".repeat(98));
    assert_eq!(width(&ascii), 101);
    assert!(
        is_too_wide(&ascii),
        "101 ASCII characters is over the limit"
    );

    // 99 characters, 291 bytes: every em-dash is three bytes.
    let dashes = format!("// {}", "—".repeat(96));
    assert_eq!(width(&dashes), 99);
    assert!(
        dashes.len() > MAX_WIDTH,
        "the fixture must be over the limit in bytes, or it pins nothing"
    );
    assert!(
        !is_too_wide(&dashes),
        "99 characters fits, whatever it weighs in bytes"
    );
}

/// Pins that the rule tells a comment from code, and in which direction.
///
/// A long string literal is not a defect: rustfmt cannot break one and does not try, so
/// reporting it would be a finding no author can act on — the kind that gets a gate
/// deleted. A long comment is the opposite: nothing else in the build will ever mention
/// it.
#[test]
fn a_long_code_line_is_not_a_violation_but_a_long_comment_is() {
    let literal = format!("    let s = \"{}\";", "x".repeat(120));
    assert!(width(&literal) > MAX_WIDTH);
    assert!(!is_too_wide(&literal), "code is rustfmt's business");

    for marker in ["//", "///", "//!"] {
        let comment = format!("    {marker} {}", "x".repeat(120));
        assert!(
            is_too_wide(&comment),
            "a {marker} line of {} characters is a violation",
            width(&comment)
        );
    }
}

/// Pins that the width includes the indent.
///
/// The limit is a column on the screen, so the measurement has to start where the line
/// starts. Measuring from the comment marker instead would pass a comment nested four
/// levels deep that runs well past the edge of the same 100-column window the code beside
/// it respects — and the deeper the nesting, the more it would be let through.
#[test]
fn the_width_is_measured_from_the_start_of_the_line_including_its_indent() {
    let body = "x".repeat(94);

    let flush = format!("// {body}");
    assert_eq!(width(&flush), 97);
    assert!(!is_too_wide(&flush));

    let indented = format!("    // {body}");
    assert_eq!(width(&indented), 101);
    assert!(
        is_too_wide(&indented),
        "the same text four columns in is over the limit"
    );
}
