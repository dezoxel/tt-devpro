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

This opens a GUI browser (Playwright, host-side — the Google OAuth flow needs a real browser window) against a persistent profile, so the Google login and its MFA are asked for once and then reused.

A cookie is written only after the portal answered `200` to that exact cookie on an authenticated endpoint, and the verified account is printed — presence in the browser profile proves nothing, since a dead cookie lingers there forever. If the saved session is rejected, `auth` drops the `.dev.pro` cookies (the Google session survives), logs in again and verifies the new one. When no session can be obtained it fails with a non-zero exit and leaves `~/.tt-cookie` untouched, rather than re-blessing a dead cookie.

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
project_ids:    # Fallback ids by DevPro project name (see below)
```

Unmapped Chrono projects are silently skipped — if entries are missing, check your mappings.

`project_ids` is a **fallback**, not an override. Project ids normally come from the portal's assigned-projects list, and that list always wins. An entry here is used only when the name is missing from it — typically because the project was renamed or unassigned — and every time one fires, `settle` prints a warning naming the project and the id it used, since a hardcoded id can quietly go stale. When a name is in neither place, `settle` still fails and lists the projects the portal does offer.

## Development

Sources live in `src/main/kotlin/pro/dev/tt/`. The Gradle wrapper (`./gradlew`)
is committed, so you don't need a system Gradle — you only need a GraalVM JDK on
`JAVA_HOME` for the native build (see [Build toolchain](#build-toolchain-one-time);
`make`/`install.sh` set it automatically from SDKMAN).

Typical loop — **edit → test → reinstall**:

```bash
# 1. make your change under src/main/kotlin/...
make test          # 2. run the suite (fast; add tests under src/test/kotlin/...)
make install       # 3. rebuild the native image AND reinstall it to ~/.local/bin
tt-devpro settle --dry-run   # 4. exercise the installed binary
```

`make install` (or `./install.sh`) always rebuilds *and* reinstalls, so the
global `tt-devpro` on your `PATH` reflects your latest changes — there is no
separate "deploy" step. Other targets:

```bash
make build     # Build the native image only (build/native/nativeCompile/tt-devpro)
make test      # Run the test suite
make clean     # Remove build artifacts
```

If you change how the app uses reflection (new serialized types, new HTTP
paths), regenerate the native-image metadata so the binary keeps working:

```bash
JAVA_HOME=$(ls -d ~/.sdkman/candidates/java/*graal* | tail -1) \
  ./gradlew -Pagent run --args="settle --dry-run"           # capture with the tracing agent
./gradlew metadataCopy --task run \
  --dir src/main/resources/META-INF/native-image             # copy config into the repo
make install                                                 # rebuild with the new metadata
```

See `CLAUDE.md` for architecture notes.
