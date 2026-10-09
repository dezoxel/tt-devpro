# tt-devpro

A single global CLI that syncs time entries from **Chrono** (local time tracker) to the **Dev.Pro Time Tracking Portal**. It reads your Chrono entries, aggregates them by date + project, normalizes each day to 8 hours (meetings preserved, work scaled), and syncs them as worklogs.

`tt-devpro` is one self-contained binary on your `PATH` — no runtime engine, no Docker.

## Install

```bash
cargo install --path .
```

Cargo builds the release binary and places it at `~/.cargo/bin/tt-devpro`, which a stock Rust install already puts on your `PATH`. `make install` runs exactly this command. There is no installer script: `ergon` reaches `PATH` the same way, and a wrapper that only shells out to `cargo install` is one more file to keep true.

The only prerequisite is a stock Rust toolchain. The crate is edition **2024** and declares `rust-version = "1.87"`, so any Rust ≥ 1.87 builds it; nothing else needs to be installed.

## Usage

```bash
tt-devpro settle                       # Interactive: review each unfilled day, approve/edit/skip
tt-devpro settle --dry-run             # Readable per-day summary of proposed actions, nothing written
tt-devpro settle --json                # Machine-readable JSON of proposed actions
tt-devpro settle --include-today       # Also settle today, whose hours aren't final yet
tt-devpro settle --from 2026-07-01 --to 2026-07-15   # Batch a specific range
```

- **Interactive** (stdin *and* stdout are both terminals): `settle` shows a draft table and asks before it writes anything. Day-by-day that is one question per unfilled day — `[A]pprove / [E]dit / [D]elete / [S]kip / [C]ancel all`; over an explicit range it is a single question for the whole range, described below.
- **Piped / non-interactive** (either stream redirected): prints the same readable summary as `--dry-run` instead of prompting.
- `--dry-run` and `--json` compute the proposals without applying them. If both are given, `--json` wins.
- Without `--from`/`--to`, `settle` runs day-by-day over a 45-day scan. With either given, it runs as a batch over the range: `--from` defaults to the 1st of the current month, `--to` to the last completed day.

**There are two interactive prompts, and `--from`/`--to` reaches the second one.** The mode comes from the arguments alone, in this order: `--json`, then `--dry-run`, then a batch run as soon as *either* end of a range is named (one end is enough, the other is defaulted), then the day-by-day scan. The day-by-day run asks once per day and accepts `[A]pprove / [E]dit / [D]elete / [S]kip / [C]ancel all`. The batch run does none of that. It prints one draft table for the whole range — one dated row per proposed worklog — and asks a single question: `[A]pprove / [C]ancel:`, or `[A]pprove anyway / [C]ancel:` when the under-8h warning fired, exactly as the day prompt swaps its own first word. There is no loop and no per-day review there, so `[A]` writes every row of the range in one go: the answer is all-or-nothing over every day in it, and the `[E]`/`[D]`/`[S]` of the day prompt do not exist at this one.

**That prompt has three branches, not two.** `a` approves and writes. `c`, or end of input, cancels and prints `Cancelled.` Anything else — a typo, an `e` or `s` carried over from the day prompt, or a bare empty line — is an unknown option and prints `Unknown option. Cancelled.` Nothing is written on either cancelling branch, so the two differ in the message and not in the outcome. What makes the difference worth knowing is that an empty line and end of input are *not* the same input — a read comes back empty-handed only at EOF, never as an empty string — and that `e` means Edit at one prompt and "cancel the entire range" at the other. The day prompt treats an unrecognised answer the same way: it cancels everything, remaining days included.

Direct portal calls live under `api`:

```bash
tt-devpro api get-projects                    # Assigned projects as of today
tt-devpro api get-projects --date 2026-07-01  # ...as of any other date
tt-devpro api get-worklogs --date 2026-07-01  # Worklogs for a period (normalView endpoint)
tt-devpro api create-worklog ...              # Create / update / delete a single worklog
```

`api get-projects` is **date-scoped, and the date is part of the answer.** The portal endpoint is `assignedProjectsOnDate` — assignments exist per date, not globally — so the command defaults to today and prints the date it queried in its header (`Assigned projects as of 2026-09-21 (32):`). This command gets used precisely when something is already wrong and its output is trusted most, so never read the list without reading the date above it.

Every command prints its own help with `--help` and exits 0; a usage failure prints the usage line plus one `Error:` line per problem and exits **1**. Note that `api create-worklog` binds `-h` to `--hours`, not to help — use the long `--help` there.

**The settle window ends at the last completed day.** `settle` proposes days that came back under 8h, and today qualifies by construction — it isn't over. Left unbounded, the filler and borrowing synthesis rounded a half-finished day up to a convincing 8h and parked it in the review table next to the legitimate one, where a single `[A]` published it. Future days got in the same way, since Chrono also holds planned entries for days that haven't started. So the default upper bound is yesterday: any date you didn't type is a completed date. `--include-today` moves the bound to today for the deliberate case (closing the books early before time off) and never past it. An explicit `--from`/`--to` is honoured verbatim, with a note on stderr if the range reaches today or beyond. The Chrono fetch deliberately reaches one day past the cutoff on the UTC axis to catch late-night local entries; every entry is then re-dated to its local day, so that padding can never introduce a future local date.

## Authentication

The portal authenticates API calls with a server-side session cookie scoped to `.dev.pro`. The cookie lives in 1Password, in the item that `session_cookie` in `~/.config/tt-devpro/config.yaml` refers to (`op://vault/item/field`), and never on disk. Every `tt-devpro` run that talks to the portal reads it with `op read`, so 1Password asks for approval each time. `$TT_COOKIE`, when set and non-empty, is used instead and 1Password is not asked.

```bash
make auth      # or: ./auth.sh
```

This opens a GUI browser (Playwright, host-side — the Google OAuth flow needs a real browser window) with an empty profile, so every run is a full Google login with MFA. Nothing of the browser session is kept between runs: a saved profile would hold a live Google session on disk in plaintext. The first run installs the Playwright version `package.json` pins and its Firefox build into `~/.cache/ms-playwright`; no system Firefox is needed.

A cookie is stored only after the portal answered `200` to that exact cookie on `/api/contact/currentUser`, and the verified account is printed. The check bypasses the browser jar and does not follow redirects, so a login page can never pose as a success. The cookie goes to `op` as item JSON on stdin, never in a command line: `op item edit` when the item exists, `op item create` from the `API Credential` template when it does not. The write counts only when `op read` of the reference then returns the same cookie. 1Password is asked about the item before the browser opens, so a locked vault or a wrong reference fails before the login rather than after it.

## Configuration (`~/.config/tt-devpro/config.yaml`)

Maps Chrono projects to DevPro projects and defines fillers/overrides:

```yaml
chrono_api: "http://localhost:9247"
vault_path: "/home/you/vault"                              # required, absolute
session_cookie: "op://Dev.Pro/TT DevPro Session/credential" # where make auth stores the cookie

mappings:
  - chrono_project: "Velocitor - DevPro - Work"
    devpro_project: "Velocitor: NLP"
    billability: "Billable"

fillers:        # Auto-fill meeting-only days with work entries
overrides:      # Reroute entries by pattern before mapping
project_ids:    # Fallback ids by DevPro project name (see below)

allocations:    # Planned billable FTE per DevPro project, for the period line (see below)
  - devpro_project: "Inveniam SOW #5"
    fte: 0.5
```

Chrono project names use the flat format (`Project - Parent - Work`) or the hierarchical slash format (`Project/Parent/Work`).

**Two different things happen to a Chrono project the config does not cover, and only one of them is silent.** Only projects whose name ends in `DevPro - Work` or `DevPro/Work` are considered at all; everything else is dropped without a word. That is the silent one, and it is why a day short on hours usually means the Chrono project is named outside that suffix rather than missing from `mappings`. A project that *does* end in the suffix but has no mapping is the opposite: the run stops with an error naming the project, printing the YAML block to paste into `~/.config/tt-devpro/config.yaml`, and listing what is configured today. Silence points at the Chrono name; a crash points at the config.

**Meeting detection reads the vault at `vault_path` off disk.** The key is required and has no default, because the vault sits at a different path on each machine; it must be absolute (`~` is not expanded). On startup `settle` walks that directory to a depth of 10 collecting every folder named `Calendar`, and uses them to decide which entries are meetings — meetings keep their actual time while work entries get scaled to reach 8h. The walk fails loudly: a root that is missing or not a directory, a folder that cannot be read, or a vault with no `Calendar` folder at all stops the run with the path at fault. The Kotlin original turned all of those into *no meetings* and scaled every meeting as work without a word.

**The billing-period line.** Under the plan, `settle` and `settle --replan` print how the current DevPro billing period is going: the period as the portal's PTR Periods view defines it (`contact/ptrPeriods`, never a 1–15 / 16–end rule — May 2026 ran 1–17), working days gone, and per billable project the hours so far — what DevPro holds plus planned lines not yet written — with their FTE. With an `allocations` entry for the project it also shows the share of the allocation used, a statusline-style bar with `┃` where the period's clock stands, and `↗N%`, the share of the allocation the period ends at if the rate holds. The portal exposes no allocation, so it lives in the config; `fte` is in (0, 1] and a project may appear once. The line is printed, not stored in `plan.md`, and reads the stored plan, so hand edits count only after `--replan`. A failed read is one `⚠️` line and the run still succeeds.

`project_ids` is a **fallback, not an override.** Project ids normally come from the portal's assigned-projects list, and that list always wins. An entry here is used only when the name is missing from it — typically because the project was renamed or unassigned — and every time one fires, `settle` warns on stderr naming the project and the id it used, since a hardcoded id can quietly go stale. A stale configured id silently beating a correct live one would post worklogs to the wrong project unnoticed, which is worse than the crash the fallback prevents. When a name is in neither place, `settle` still fails and lists the projects the portal does offer.

## Development

Sources live in `src/`, tests in `src/` (unit) and `tests/` (integration). Typical loop — **edit → test → reinstall**:

```bash
# 1. make your change under src/...
make test          # 2. run the suite
make install       # 3. rebuild the release binary AND reinstall it to ~/.cargo/bin
tt-devpro settle --dry-run   # 4. exercise the installed binary
```

`make install` always rebuilds *and* reinstalls, so the global `tt-devpro` on your `PATH` reflects your latest changes — there is no separate "deploy" step. Other targets:

```bash
make build     # Build the release binary only (target/release/tt-devpro)
make test      # Run the test suite
make clean     # Remove build artifacts
```

The gates the suite is expected to pass clean:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo doc --no-deps --document-private-items
```

`cargo doc` is in that list because it is the only one of the four that runs rustdoc at all. The sources carry rustdoc links in place of line numbers, and the crate root's `#![deny(warnings)]` does reach them — it implies `deny(rustdoc::broken_intra_doc_links)`, so a link naming an element that has since been renamed is a hard error rather than a warning. But that error only ever exists while something is documenting the crate, and neither `cargo test` nor `cargo clippy` does. `--document-private-items` is what points rustdoc at the private half of the tree, which is where most of those links live.

`cargo fmt` holds code to 100 columns and leaves comments at whatever width they were typed: the option that would wrap them, `wrap_comments`, is nightly-only and off by default. An over-wide comment is therefore seen by nothing in that list — not `cargo test`, not clippy, not `cargo fmt --check`, not `cargo doc`. The blind spot is measurable rather than theoretical: the rounds that gave the citations their file names took this tree from 5 over-wide comment lines to 67, with every gate green throughout, and a later wrap pass got it back down to 49 and stopped there. `tests/comment_width.rs` closes it. It measures every line under `src/` and `tests/` whose indent is followed by `//`, and fails with all the offenders at once — path, line number, width. Rewrap those by hand; rustfmt will not do it for you. There is one exemption and it is narrow: a comment whose text is a single token with no space in it — a path, a URL — has nowhere to wrap, and the limit would be asking the author for something they cannot give. Two lines in `src/main.rs` rely on it, both the same absolute measurement path under `~/.cache/`. Prose cannot reach the exemption, because prose has spaces in it. Measure with `chars().count()` rather than `awk 'length>100'`: macOS awk counts bytes, and counting those 49 lines with it reported 58, nine of them blamed for the width of their own em-dashes.

## Resolving the `*.kt` citations in the sources

The Rust sources and tests carry citations of the form `SomeFile.kt:NNN`, each pointing at the Kotlin implementation the behaviour was derived from. **That Kotlin tree is no longer on any branch.** It was deleted when the Rust port took over, and its final state is commit **`06fb43e`**, which is the only place those line numbers resolve.

Every such citation — both the ones naming implementation files under `src/main/kotlin/` and the ones naming test files under `src/test/kotlin/` — refers to that tree at `06fb43e`. Read any cited file with:

```bash
git show 06fb43e:<path>
```

Worked examples, one from each half of the tree:

```bash
git show 06fb43e:src/main/kotlin/pro/dev/tt/commands/SettleCommand.kt
git show 06fb43e:src/test/kotlin/pro/dev/tt/SettleWindowTest.kt
```

To jump straight to a cited line, pipe it: `git show 06fb43e:<path> | sed -n '197p'`.

`cargo test` checks all of this for you. `tests/citations.rs` resolves every `SomeFile.kt:NNN` in `src/` against `06fb43e` and fails on any that names a file the pin does not carry, or a line the file does not reach. It is a test rather than a one-off audit because a wrong citation is not a loud failure: `git show` answers a bad line with real, unrelated Kotlin, so the reader who follows it is misled rather than stopped — and nothing about it looks different from the citations that are right.

The same test rejects a citation that leaves its file out — a bare `:NNN`, to be resolved against whichever Kotlin file the surrounding Rust happens to port. That inference is not as safe as it reads: when the citations were last counted by hand, 204 of them omitted the file and 27 of those pointed somewhere other than where their author meant, each one landing on a real line of a real file. So name the file every time. It is what turns a citation from a guess the reader has to make into something the suite can check.
