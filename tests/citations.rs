//! The citation gate.
//!
//! The Rust sources cite the deleted Kotlin tree as `SomeFile.kt:NNN`. The only place
//! those numbers resolve is the pinned commit `06fb43e`, and nothing in the build ever
//! looked at them — so a citation pointing at the wrong file, or past the end of the
//! right one, reads exactly like a correct one. Worse, `git show` answers a wrong
//! number with confident unrelated Kotlin rather than with silence, so the reader who
//! follows it is misled rather than stopped.
//!
//! An audit can only say how many were broken on the day it ran. This tree has already
//! demonstrated the decay: four citations in `src/service/aggregator.rs` were written
//! against a `ProjectIdResolutionTest.kt` that no longer existed at the cutover, and the
//! commits that followed the audit added five fresh bare citations to `settle.rs`. Two
//! tests here turn the snapshot into a gate.
//!
//! Scope is `src/` recursively, every `*.rs`. `tests/` is not scanned: an integration
//! test that quotes a citation form in order to describe it is not itself a citation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The commit the Kotlin tree was deleted at. Every `SomeFile.kt:NNN` in `src/` means
/// a line of this commit and of no other.
const PIN: &str = "06fb43e";

fn repo_root() -> PathBuf {
    // Not the current directory: `cargo test` may run the binary from anywhere, while
    // the manifest directory is the repository by construction.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Runs `git` in the repository root and returns its stdout.
///
/// Every failure here is fatal on purpose. `install.sh` builds this project in place
/// out of its own checkout, so a missing `git` or an unresolvable pin is a broken
/// repository rather than somebody else's environment — and a gate that skips itself
/// when it cannot check is the swept rug it was written to remove.
fn git(args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo_root())
        .args(args)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "citation gate: could not run `git {}`: {e}\n\
                 The Kotlin tree exists only at {PIN}, so without git nothing here can be checked.",
                args.join(" ")
            )
        });
    assert!(
        out.status.success(),
        "citation gate: `git {}` failed ({})\nstderr: {}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The pinned Kotlin tree, indexed the way a citation addresses it: by bare file name.
struct PinnedTree {
    /// File name (`SettleCommand.kt`) to every path at the pin carrying that name.
    /// A name with two paths is a defect in its own right — the citation form cannot
    /// say which one it means — so the vector is kept rather than collapsed.
    by_name: BTreeMap<String, Vec<String>>,
    /// Path to its line count at the pin, filled lazily: 23 Kotlin files answer more
    /// than 700 citations, so one `git show` per citation would make the gate crawl.
    lines: BTreeMap<String, usize>,
}

impl PinnedTree {
    fn load() -> Self {
        let kind = git(&["cat-file", "-t", PIN]);
        assert_eq!(
            kind.trim(),
            "commit",
            "citation gate: {PIN} is not a commit in this repository (got {:?}). \
             Every Kotlin citation in src/ is measured into that commit.",
            kind.trim()
        );

        let listing = git(&["ls-tree", "-r", "--name-only", PIN]);
        let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for path in listing.lines().filter(|p| p.ends_with(".kt")) {
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            by_name.entry(name).or_default().push(path.to_string());
        }
        assert!(
            !by_name.is_empty(),
            "citation gate: `git ls-tree -r --name-only {PIN}` listed no .kt file. \
             The pin is supposed to be the final state of the Kotlin tree."
        );

        Self {
            by_name,
            lines: BTreeMap::new(),
        }
    }

    fn line_count(&mut self, path: &str) -> usize {
        if let Some(n) = self.lines.get(path) {
            return *n;
        }
        let body = git(&["show", &format!("{PIN}:{path}")]);
        let n = body.lines().count();
        self.lines.insert(path.to_string(), n);
        n
    }
}

/// One span of cited lines. A single number is a span of length one, so the range
/// ordering check (`from <= to`) is the only shape the rest of the file has to know.
#[derive(Clone, Copy)]
struct Span {
    from: usize,
    to: usize,
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.from == self.to {
            write!(f, "{}", self.from)
        } else {
            write!(f, "{}-{}", self.from, self.to)
        }
    }
}

/// A citation that names the file it points into.
struct Named {
    /// The Kotlin file name, e.g. `SettleCommand.kt`.
    name: String,
    /// The citation exactly as written, e.g. `SettleCommand.kt:486,490,494-496`.
    text: String,
    spans: Vec<Span>,
}

fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Parses `NNN`, `NNN-MMM` and `NNN,MMM,KKK-LLL` starting at `pos`.
///
/// A separator only continues the list when a digit follows it immediately. That is
/// what keeps `` `SettleCommand.kt:99`, `:286` `` from being read as one three-number
/// citation, and what stops an em-dash of prose after a number from opening a range.
fn parse_number_list(b: &[u8], mut pos: usize) -> (Vec<Span>, usize) {
    let mut spans = Vec::new();
    loop {
        let Some((from, after)) = parse_number(b, pos) else {
            break;
        };
        pos = after;
        let mut to = from;
        if b.get(pos) == Some(&b'-') {
            if let Some((end, after_end)) = parse_number(b, pos + 1) {
                to = end;
                pos = after_end;
            }
        }
        spans.push(Span { from, to });
        if b.get(pos) == Some(&b',') && b.get(pos + 1).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        } else {
            break;
        }
    }
    (spans, pos)
}

fn parse_number(b: &[u8], pos: usize) -> Option<(usize, usize)> {
    let mut end = pos;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    if end == pos {
        return None;
    }
    let n = std::str::from_utf8(&b[pos..end]).ok()?.parse().ok()?;
    Some((n, end))
}

/// Every `SomeFile.kt:NNN...` on one line.
fn named_citations(line: &str) -> Vec<Named> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= b.len() {
        if &b[i..i + 4] == b".kt:" {
            let mut start = i;
            while start > 0 && is_name_byte(b[start - 1]) {
                start -= 1;
            }
            if start < i {
                let (spans, end) = parse_number_list(b, i + 4);
                if !spans.is_empty() {
                    out.push(Named {
                        name: format!("{}.kt", &line[start..i]),
                        text: line[start..end].to_string(),
                        spans,
                    });
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Every citation on one line that leaves its file to be inferred: `` `:NNN` ``,
/// `` `:NNN-MMM` ``, `` `:NNN,MMM` `` and the unbackticked `// :NNN-MMM`.
///
/// A colon-then-digit counts as bare unless the character in front of it could belong
/// to a file name — a letter, a digit, `_` or `.` — which is what keeps `Models.kt:118`
/// and `settle.rs:1924` out of the result. A double quote is excluded for a different
/// reason: JSON keys in the test fixtures (`"duration":0.5`) and in the comment that
/// quotes one have the same shape and are not citations.
fn bare_citations(line: &str) -> Vec<String> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let carries_a_name = i > 0
                && (b[i - 1].is_ascii_alphanumeric()
                    || b[i - 1] == b'_'
                    || b[i - 1] == b'.'
                    || b[i - 1] == b'"');
            if !carries_a_name {
                let (_, end) = parse_number_list(b, i + 1);
                out.push(line[i..end].to_string());
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Every `*.rs` under `src/`, sorted, so a failing run lists its findings in the same
/// order every time and a fixer can work straight down the list.
fn rust_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_rust(&repo_root().join("src"), &mut out);
    assert!(
        !out.is_empty(),
        "citation gate: found no .rs file under src/ — the scan would pass vacuously."
    );
    out.sort();
    out
}

fn collect_rust(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("citation gate: cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("citation gate: cannot read {}: {e}", dir.display()))
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

/// Reports every violation at once, one per line.
///
/// Failing on the first one would turn a fix into one `cargo test` run per defect, and
/// the audit that prompted this gate found 27 at a time.
fn report(header: &str, violations: &[String]) {
    assert!(
        violations.is_empty(),
        "{} {}:\n{}\n",
        violations.len(),
        header,
        violations.join("\n")
    );
}

/// Catches a citation that names a file but points nowhere in it.
///
/// Without this test, `FillerService.kt:999` against a 167-line file passes the build
/// untouched, and so does a name no longer in the tree — which is how four
/// `ProjectIdResolutionTest.kt` ranges survived the cutover pointing into a version of
/// that file that had already been rewritten. Both defects are invisible to a reader
/// who does not run `git show` themselves, and `git show` on an out-of-range line is
/// silent rather than loud.
#[test]
fn every_kotlin_citation_resolves_to_a_line_that_exists_at_the_pin() {
    let mut pin = PinnedTree::load();
    let mut violations = Vec::new();
    let mut checked = 0usize;

    for source in rust_sources() {
        let body = fs::read_to_string(&source)
            .unwrap_or_else(|e| panic!("citation gate: cannot read {}: {e}", source.display()));
        let rust = relative(&source);

        for (index, line) in body.lines().enumerate() {
            let at = format!("{rust}:{}", index + 1);
            for citation in named_citations(line) {
                checked += 1;
                let Some(paths) = pin.by_name.get(&citation.name) else {
                    violations.push(format!(
                        "{at}: {} — no file named {} exists at {PIN}",
                        citation.text, citation.name
                    ));
                    continue;
                };
                if paths.len() > 1 {
                    violations.push(format!(
                        "{at}: {} — {} is ambiguous at {PIN}, {} paths carry that name: {}",
                        citation.text,
                        citation.name,
                        paths.len(),
                        paths.join(", ")
                    ));
                    continue;
                }
                let path = paths[0].clone();
                let len = pin.line_count(&path);
                for span in &citation.spans {
                    if span.from > span.to {
                        violations.push(format!(
                            "{at}: {} — range {span} runs backwards",
                            citation.text
                        ));
                    } else if span.from == 0 || span.to > len {
                        violations.push(format!(
                            "{at}: {} — {} has {len} lines at {PIN}",
                            citation.text, citation.name
                        ));
                    }
                }
            }
        }
    }

    // A parser that stopped matching would report nothing and read as a pass. The
    // tree carries hundreds of named citations, so zero means the gate broke, not
    // that the citations got better.
    assert!(
        checked > 0,
        "citation gate: scanned src/ and found no `SomeFile.kt:NNN` at all — \
         the parser, not the tree, is what changed."
    );
    report("citations do not resolve at the pin", &violations);
}

/// Catches a citation that leaves its file name to be inferred from context.
///
/// Without this test, `` `:125-127` `` passes, and the reader resolves it against
/// whatever Kotlin file they believe the surrounding Rust ports. The audit measured
/// that guess wrong 27 times out of 204, and — this is the part the resolution test
/// above cannot see — every one of those 27 lands on real Kotlin at a real line, so
/// no arithmetic can flag them. The only durable fix is that the form does not exist:
/// an author who types `FillerService.kt:125-127` cannot produce
/// `SettleCommand.kt:125-127` by leaving the file out.
#[test]
fn no_citation_omits_the_file_it_points_into() {
    let mut violations = Vec::new();

    for source in rust_sources() {
        let body = fs::read_to_string(&source)
            .unwrap_or_else(|e| panic!("citation gate: cannot read {}: {e}", source.display()));
        let rust = relative(&source);

        for (index, line) in body.lines().enumerate() {
            for bare in bare_citations(line) {
                violations.push(format!(
                    "{rust}:{}: `{bare}` names no file — {}",
                    index + 1,
                    line.trim()
                ));
            }
        }
    }

    report("citations omit the file they point into", &violations);
}

/// Pins what the named-citation parser reads, so the gate cannot pass by finding nothing.
///
/// Without this test, a parser that quietly stopped recognising the comma form, or one
/// that swallowed the `` `:286` `` of a following bare citation into the range of the
/// named one before it, would report zero violations and look exactly like a clean tree.
/// Both mistakes are one character wide in [`parse_number_list`].
#[test]
fn the_named_form_is_read_exactly_as_far_as_it_is_written() {
    let render = |line: &str| {
        named_citations(line)
            .iter()
            .map(|c| {
                let spans: Vec<String> = c.spans.iter().map(Span::to_string).collect();
                format!("{}@{}", c.name, spans.join(","))
            })
            .collect::<Vec<_>>()
            .join(" ")
    };

    assert_eq!(render("/// `Models.kt:118,120`"), "Models.kt@118,120");
    assert_eq!(
        render("/// `SettleCommand.kt:486,490,494-496`"),
        "SettleCommand.kt@486,490,494-496"
    );
    // The bare `:286` belongs to the other test; it must not extend the range above it.
    assert_eq!(
        render("/// `SettleCommand.kt:99`, `:286`"),
        "SettleCommand.kt@99"
    );
    // A file name with no line number is a path, not a citation.
    assert_eq!(render("/// git show 06fb43e:.../Models.kt"), "");
    assert_eq!(render("/// nothing to see here"), "");
}

/// Pins what the bare-citation parser treats as a citation, and what it does not.
///
/// Without this test, widening the rule far enough to catch `// :51-54` also catches
/// `"duration":0.5` in the JSON fixtures — a failure the tree cannot fix, which is the
/// kind of false alarm that gets a gate deleted. Narrowing it far enough to spare the
/// fixtures can also spare `` `:33` ``, which is the defect the gate exists for.
#[test]
fn the_bare_form_is_recognised_and_a_json_key_is_not() {
    assert_eq!(bare_citations("/// `:33` is the range arm"), vec![":33"]);
    assert_eq!(bare_citations("// :51-54 — flag first"), vec![":51-54"]);
    assert_eq!(bare_citations("//! `:605,620`"), vec![":605,620"]);
    assert_eq!(
        bare_citations("//!    replace by identity (`:774`, `:820`)"),
        vec![":774", ":820"]
    );

    // Carries its file: not this test's business either way.
    assert!(bare_citations("/// `Models.kt:118,120`").is_empty());
    assert!(bare_citations("/// `settle.rs:1924-1927`").is_empty());
    // A JSON key in a fixture, and the comment that quotes one.
    assert!(bare_citations(r#"body.contains("\"duration\":2.5")"#).is_empty());
    assert!(bare_citations(r#"/// `"duration":0.5` as a number"#).is_empty());
}
