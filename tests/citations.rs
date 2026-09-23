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
//! commits that followed the audit added five fresh bare citations to `settle.rs`. Three
//! tests here turn the snapshot into a gate: every Kotlin citation resolves at the pin,
//! none omits the file it points into, and none points into Rust by line number at all —
//! that last form has no version a build can check, and a live file moves under it.
//!
//! Scope is every `*.rs` under `src/` and `tests/`, recursively, minus one named file:
//! this one. A fourth test pins that the exemption stays one file wide — see `EXEMPT`
//! for why the reason is peculiar to this file, and
//! [`the_scan_exempts_this_file_alone_and_reads_every_other_test`] for what holds it
//! there.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The commit the Kotlin tree was deleted at. Every `SomeFile.kt:NNN` under the scan
/// roots means a line of this commit and of no other.
const PIN: &str = "06fb43e";

/// The directories the scan reads, each one recursively.
///
/// `tests/` is in scope because a citation in a test is a citation. The six
/// `ApiCommand.kt:NNN` in `tests/cli.rs` are the stated reason an assertion expects the
/// order and the wording it expects, a reader follows them exactly as they follow the
/// ones in `src/`, and they rot the same way — the Kotlin they address is frozen at the
/// pin while the prose around them is edited freely.
const SCAN_ROOTS: &[&str] = &["src", "tests"];

/// The files the scan does not read, named one at a time.
///
/// Exactly one file is here and the reason belongs to it alone: this file's body is
/// where the citation forms are written down in order to be described.
/// `FillerService.kt:999` on the resolution test is an out-of-range number on purpose,
/// the parser fixtures carry `` `:33` `` and `settle.rs:1925` because those are the
/// forms they pin, and every one of them would be reported as the defect it is
/// imitating. There is no way to write the fixture correctly, because being incorrect
/// is what the fixture is.
///
/// `tests/cli.rs` is the opposite, and is why this is a file name rather than a
/// directory. Its `ApiCommand.kt:74` points at the Kotlin that justifies the assertion
/// beside it — a citation in the ordinary sense, load-bearing for a reader deciding
/// whether the expectation is right. Stating the exemption as "skip `tests/`" took a
/// reason true of one file and applied it to a directory, and that is what left those
/// six unchecked for as long as this gate has existed.
const EXEMPT: &[&str] = &["tests/citations.rs"];

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

/// A citation that names the file it points into, Kotlin or Rust.
struct Cited {
    /// The file name, e.g. `SettleCommand.kt` or `settle.rs`.
    name: String,
    /// The citation exactly as written, e.g. `SettleCommand.kt:486,490,494-496`.
    text: String,
    spans: Vec<Span>,
    /// Byte index just past the citation, so a caller can look at what follows it.
    /// That is the whole of how a panic location is told from a citation.
    end: usize,
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

/// Every `SomeFile<ext>:NNN...` on one line, where `dot_ext_colon` is `.kt:` or `.rs:`.
///
/// Kotlin and Rust citations have the same shape and differ only in the extension, so
/// they are read by the same scanner. What the two callers do with the result differs
/// completely — one resolves it, the other forbids it — but neither wants its own
/// copy of the backwards walk over the file name.
fn citations_named(line: &str, dot_ext_colon: &str) -> Vec<Cited> {
    let b = line.as_bytes();
    let needle = dot_ext_colon.as_bytes();
    let ext = &dot_ext_colon[..dot_ext_colon.len() - 1];
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= b.len() {
        if &b[i..i + needle.len()] == needle {
            let mut start = i;
            while start > 0 && is_name_byte(b[start - 1]) {
                start -= 1;
            }
            if start < i {
                let (spans, end) = parse_number_list(b, i + needle.len());
                if !spans.is_empty() {
                    out.push(Cited {
                        name: format!("{}{ext}", &line[start..i]),
                        text: line[start..end].to_string(),
                        spans,
                        end,
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

/// Every `SomeFile.kt:NNN...` on one line.
fn kotlin_citations(line: &str) -> Vec<Cited> {
    citations_named(line, ".kt:")
}

/// Every citation on one line that points into a Rust file by line number, such as
/// `settle.rs:1925` or `settle.rs:1924-1927`.
///
/// A `:LINE:COL` tail marks a panic location rather than a citation — `panicked at
/// src/foo.rs:12:5`, a `file!()`/`line!()` pair pasted into a message, a backtrace
/// pinned in an assertion — so a number followed by another colon and a digit is
/// dropped. All three shapes carry the column, and that one condition is why this
/// rule does not have to tell a comment from a string literal. A `.rs` with no line
/// number at all — the path in a `git show` example — never starts a citation.
fn rust_line_citations(line: &str) -> Vec<String> {
    let b = line.as_bytes();
    citations_named(line, ".rs:")
        .into_iter()
        .filter(|cited| {
            let carries_a_column = b.get(cited.end) == Some(&b':')
                && b.get(cited.end + 1).is_some_and(u8::is_ascii_digit);
            !carries_a_column
        })
        .map(|cited| cited.text)
        .collect()
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

/// Whether a path is named in [`EXEMPT`], compared as a repository-relative path so
/// that `tests/citations.rs` cannot be matched by a `citations.rs` somewhere else.
fn is_exempt(path: &Path) -> bool {
    EXEMPT.contains(&relative(path).as_str())
}

/// Every `*.rs` under the scan roots, exemptions dropped, sorted, so a failing run lists
/// its findings in the same order every time and a fixer can work straight down the list.
fn rust_sources() -> Vec<PathBuf> {
    let root = repo_root();
    let mut out = Vec::new();
    for dir in SCAN_ROOTS {
        let mut found = Vec::new();
        collect_rust(&root.join(dir), &mut found);
        found.retain(|path| !is_exempt(path));
        // Emptiness is asserted per root, not over the total. `src/` alone holds 20 of
        // the 22 files scanned, so a `tests/` that stopped being walked — moved, or
        // emptied one exemption at a time — would leave the total looking healthy while
        // the half of the scope this gate was just widened to reached nothing at all.
        assert!(
            !found.is_empty(),
            "citation gate: no scannable .rs file under {dir}/ — that half would pass vacuously."
        );
        out.append(&mut found);
    }
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

/// Reports every violation at once, one per line, and prints `remedy` once underneath
/// rather than repeating it on each line.
///
/// Failing on the first violation would turn a fix into one `cargo test` run per defect,
/// and the audit that prompted this gate found 27 at a time.
fn report(header: &str, violations: &[String], remedy: &str) {
    if violations.is_empty() {
        return;
    }
    // The count trails the header rather than leading it, so one violation does not
    // read as "1 citations".
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
            for citation in kotlin_citations(line) {
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
    report("citations do not resolve at the pin", &violations, "");
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

    report("citations omit the file they point into", &violations, "");
}

/// Catches a citation that points into a Rust file by line number.
///
/// Without this test the form is unchecked by anything. The resolution test above only
/// reads `*.kt`, the bare-form test above skips anything carrying a file name, and
/// `cargo doc` never sees it because it is not a link. The form also has no safe
/// version: the Kotlin pin is frozen, so a number there is either right or out of
/// range, while a Rust file is live and every line inserted above a citation moves its
/// target while leaving the number in range. That is not hypothetical — `normalizer.rs`
/// once carried `:223` for a field read that lives at `:229`, in a different function
/// from the one its prose named, and no arithmetic could have flagged it.
///
/// The form has come back once already, introduced by the very commit that was fixing
/// citations, which is the argument for forbidding it rather than checking it.
#[test]
fn no_citation_points_into_rust_by_line_number() {
    let mut violations = Vec::new();

    for source in rust_sources() {
        let body = fs::read_to_string(&source)
            .unwrap_or_else(|e| panic!("citation gate: cannot read {}: {e}", source.display()));
        let rust = relative(&source);

        for (index, line) in body.lines().enumerate() {
            for citation in rust_line_citations(line) {
                violations.push(format!(
                    "{rust}:{}: `{citation}` cites Rust by line — {}",
                    index + 1,
                    line.trim()
                ));
            }
        }
    }

    report(
        "citations point into Rust by line number",
        &violations,
        "Name the element instead. Either a rustdoc link — [`run_batch_mode`], which \
         cargo doc resolves and the crate's #![deny(warnings)] turns into an error, \
         though only outside #[cfg(test)], where rustdoc does not look — or the element \
         in prose with no number at all, e.g. \"the !io.present() branch of \
         run_batch_mode\", which cannot go stale because there is nothing in it to shift.",
    );
}

/// Catches the exemption growing, going stale, or swallowing the directory again.
///
/// The scope this file now has is one line of code away from the scope it replaced:
/// walking `tests/` and filtering has the same shape as not walking `tests/` at all,
/// and the only thing between them is the contents of [`EXEMPT`], which nothing else
/// reads. Without this test a second entry — added to quiet whichever file failed next
/// — would widen the hole back to a directory one name at a time, in silence.
///
/// Five things are asserted and each can fail alone. The list is exactly the one name,
/// so it cannot grow. That name is a file that exists, so a rename cannot leave an
/// exemption guarding nothing. The file is really kept out of the scan, so the entry is
/// not decorative. `tests/cli.rs` is really in it, which is the hole the scope was
/// widened to close and the one assertion here measuring the change rather than the
/// guard around it. And the exempt file carries all three forms the tests above report,
/// so the exemption is load-bearing — a clean file parked in the list would pass the
/// four assertions before this one and fail it.
#[test]
fn the_scan_exempts_this_file_alone_and_reads_every_other_test() {
    assert_eq!(
        EXEMPT,
        ["tests/citations.rs"],
        "the exemption answers one situation: a file whose body writes the citation \
         forms down in order to describe them. No other file in this tree does that."
    );

    let scanned: Vec<String> = rust_sources().iter().map(|path| relative(path)).collect();

    for &exempt in EXEMPT {
        assert!(
            repo_root().join(exempt).is_file(),
            "citation gate: {exempt} is exempted and is not a file. An exemption naming \
             something that has moved guards nothing and hides the move."
        );
        assert!(
            !scanned.contains(&exempt.to_string()),
            "citation gate: {exempt} is named in EXEMPT and was scanned anyway."
        );
    }

    assert!(
        scanned.contains(&"tests/cli.rs".to_string()),
        "citation gate: tests/cli.rs is outside the scan. Its Kotlin citations are the \
         hole this scope was widened to close. Scanned: {scanned:?}"
    );

    // Each of the three forms is counted in the exempt file rather than assumed to be
    // there. That is what tells an exemption written for a reason from one written to
    // make a failure go away.
    let mut pin = PinnedTree::load();
    let body = fs::read_to_string(repo_root().join(EXEMPT[0]))
        .unwrap_or_else(|e| panic!("citation gate: cannot read {}: {e}", EXEMPT[0]));
    let mut unresolvable = 0usize;
    let mut bare = 0usize;
    let mut into_rust = 0usize;
    for line in body.lines() {
        for citation in kotlin_citations(line) {
            let path = pin
                .by_name
                .get(&citation.name)
                .map(|paths| paths[0].clone());
            let broken = match path {
                None => true,
                Some(path) => {
                    let len = pin.line_count(&path);
                    citation.spans.iter().any(|span| span.to > len)
                }
            };
            if broken {
                unresolvable += 1;
            }
        }
        bare += bare_citations(line).len();
        into_rust += rust_line_citations(line).len();
    }

    assert!(
        unresolvable > 0 && bare > 0 && into_rust > 0,
        "citation gate: {} carries {unresolvable} unresolvable Kotlin citations, {bare} \
         bare ones and {into_rust} pointing into Rust by line. All three have to be \
         there, or this file is clean and its exemption is hiding nothing.",
        EXEMPT[0]
    );
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
        kotlin_citations(line)
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

/// Pins the one condition that lets the Rust rule skip telling a comment from a string.
///
/// Without this test the rule is either too wide or too narrow, and both failures are
/// silent. Too wide, it fires on `panicked at src/foo.rs:12:5` and on anything built
/// from `file!()`/`line!()` — findings no author can fix, which is how a gate earns its
/// deletion. Too narrow — say, requiring a backtick, or a leading `//` — and it stops
/// seeing the citations it exists for, reports zero and reads as a clean tree. The
/// column is the whole discriminator, so it is the thing worth pinning.
#[test]
fn the_rust_rule_reads_a_citation_but_not_a_panic_location() {
    assert_eq!(
        rust_line_citations("/// C7 on the batch surface (`settle.rs:1925`)."),
        vec!["settle.rs:1925"]
    );
    assert_eq!(
        rust_line_citations("/// see settle.rs:1924-1927 and normalizer.rs:229"),
        vec!["settle.rs:1924-1927", "normalizer.rs:229"]
    );
    // A path carries the file name too, and the line number is what makes it a citation.
    assert_eq!(
        rust_line_citations("/// src/service/aggregator.rs:129 keeps only the suffix"),
        vec!["aggregator.rs:129"]
    );

    // Panic locations: the column is what tells them apart, in a fixture or in prose.
    assert!(rust_line_citations("panicked at src/foo.rs:12:5:").is_empty());
    assert!(
        rust_line_citations(r#"assert!(msg.contains("src/commands/settle.rs:1925:9"))"#).is_empty()
    );
    // A `.rs` inside a path with no line number is not a citation.
    assert!(rust_line_citations("/// git show HEAD:src/commands/settle.rs").is_empty());
    assert!(rust_line_citations("/// tests/cli.rs runs against the built binary").is_empty());
    // Kotlin citations are the other test's business.
    assert!(rust_line_citations("/// `SettleCommand.kt:910`").is_empty());
}
