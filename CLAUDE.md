# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

tt-devpro is a Kotlin CLI tool for synchronizing time entries from Chrono (local time tracking) to DevPro Time Tracking Portal.

## Build & Run Commands

**IMPORTANT: Always run via Docker using Makefile commands.**

### Usage Mode (pre-built image)
```bash
make build        # Build production Docker image (once or after code changes)
make tt settle    # Run CLI command with pre-built image
make tt settle --from 2025-12-01 --to 2025-12-15
```

### Dev Mode (hot reload)
```bash
make dev-start    # Start dev environment with hot reload
make tt settle    # Runs via docker exec (picks up code changes automatically)
make dev-stop     # Stop dev container
make dev-logs     # View container logs
```

### Other
```bash
make auth         # Refresh DevPro token via browser (runs on host)
make clean        # Remove containers, volumes, and images
```

**Note:** `make tt` auto-detects mode - uses dev container if running, otherwise uses pre-built image.

## CLI Commands

### The `settle` Command

The main (and only) command. The name "settle" was chosen because it reflects what the tool actually does:

- **"Settle accounts"** — like closing books in accounting, finalizing the day's time entries
- **"Settle the day"** — normalize hours to exactly 8h, fill gaps, and finalize
- Implies correctness and finality — the day is "settled" and ready for reporting

This is not just a sync tool. It performs intelligent processing:
1. **Aggregates** Chrono entries by date+project
2. **Normalizes** hours to exactly 8h/day (meetings preserved, work scaled proportionally)
3. **Auto-fills** meeting-only days with predefined filler activities
4. **Borrows** tasks from 7-day history when fillers aren't enough
5. **Reconciles** with existing DevPro worklogs (CREATE vs UPDATE)

**Usage:**
- `tt settle` — Interactive day-by-day mode (checks last 45 days)
- `tt settle --from YYYY-MM-DD --to YYYY-MM-DD` — Batch mode for date range

Note: Use `make auth` to refresh token (runs on host with browser, not in Docker).

## Architecture

### Data Flow (settle command)
1. Fetch time entries from Chrono API
2. Aggregate entries by date+project using config mappings
3. Normalize hours to 8h/day (proportional scaling, meetings preserved)
4. Auto-fill meeting-only days with filler activities
5. Borrow tasks from history for remaining gaps
6. Generate task titles from Chrono descriptions
7. Match with existing DevPro worklogs (create or update)
8. Interactive review and apply

### Key Components

- **API Clients** (`api/`): TtApiClient (DevPro), ChronoClient
- **Config** (`config/`): YAML config loader for `~/.tt-config.yaml`
- **Commands** (`commands/`): Clikt-based CLI commands
- **Services** (`service/`): Aggregator, TimeNormalizer, FillerService, BorrowerService

### Configuration

Authentication: `~/.tt-token` or `TT_TOKEN` env var

Config file `~/.tt-config.yaml`:
```yaml
chrono_api: "http://localhost:9247"

mappings:
  - chrono_project: "Your Chrono Project"
    devpro_project: "DevPro Project Name"
    billability: "Billable"

fillers:
  - devpro_project: "Research"
    task_title: "R&D Work"
    billability: "Internal"
    min_hours: 0.5
    max_hours: 2.0
```

### Tech Stack

- Kotlin 1.9, JDK 17
- Clikt (CLI framework)
- Ktor Client (HTTP)
- kotlinx.serialization + kaml (JSON/YAML)
