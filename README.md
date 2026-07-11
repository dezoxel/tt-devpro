# tt-devpro

A single global CLI that syncs time entries from **Chrono** (local time tracker) to the **Dev.Pro Time Tracking Portal**. It reads your Chrono entries, aggregates them by date + project, normalizes each day to 8 hours (meetings preserved, work scaled), and syncs them as worklogs.

No Docker, no Gradle-on-PATH at runtime — `tt-devpro` is a self-contained binary on your `PATH`.

## Install

```bash
./install.sh
```

This builds and installs `tt-devpro` to `~/.local/bin/`:

- **Native** (preferred) — a single self-contained binary via GraalVM `native-image`, no JVM needed at runtime. Requires a GraalVM JDK; the script auto-detects one installed via SDKMAN.
- **JVM fallback** — `./install.sh --jvm` (or automatic if native-image is unavailable) builds a `gradle installDist` launcher that runs on any host JDK ≥ 17.

Ensure `~/.local/bin` is on your `PATH`.

### Build toolchain (one-time)

The native build needs a GraalVM JDK 21. Simplest, no-sudo path:

```bash
curl -s "https://get.sdkman.io" | bash
sdk install java 21.0.11-graal
```

`install.sh` and the `Makefile` pick this up automatically via `JAVA_HOME`.

## Usage

```bash
tt-devpro settle                       # Interactive: review each unfilled day, approve/edit/skip
tt-devpro settle --dry-run             # Readable per-day summary of proposed actions, nothing written
tt-devpro settle --json                # Machine-readable JSON of proposed actions
tt-devpro settle --from 2026-07-01 --to 2026-07-15   # Batch a specific range
```

- **Interactive** (a TTY): steps through each unfilled day for `[A]pprove / [E]dit / [D]elete / [S]kip`.
- **Piped / non-interactive**: prints the same readable summary as `--dry-run` instead of prompting.
- `--dry-run` and `--json` compute the proposals without applying them. If both are given, `--json` wins.

## Authentication

The portal authenticates API calls with a server-side session cookie (scoped to `.dev.pro`, ~2-week lifetime), saved to `~/.tt-cookie`.

```bash
make auth      # or: ./auth.sh
```

This opens a GUI browser (Playwright, host-side — the Google OAuth flow needs a real browser window), waits for you to log in the first time (the session then persists), and extracts the cookie to `~/.tt-cookie`.

## Configuration (`~/.tt-config.yaml`)

Maps Chrono projects to DevPro projects and defines fillers/overrides:

```yaml
chrono_api: "http://localhost:9247"

mappings:
  - chrono_project: "Velocitor - DevPro - Work"
    devpro_project: "Velocitor: NLP"
    billability: "Billable"

fillers:        # Auto-fill meeting-only days with work entries
overrides:      # Reroute entries by pattern before mapping
project_ids:    # DevPro project ID overrides
```

Unmapped Chrono projects are silently skipped — if entries are missing, check your mappings.

## Development

```bash
make build     # Build the native image
make test      # Run the test suite
make clean     # Remove build artifacts
```

See `CLAUDE.md` for architecture notes.
