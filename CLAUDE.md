# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

tt-devpro is a Kotlin CLI tool for synchronizing time entries from Chrono (local time tracking) to DevPro Time Tracking Portal.

## Build & Run Commands

**IMPORTANT: Always run via Docker using Makefile commands.**

### Usage Mode (pre-built image)
```bash
make build        # Build production Docker image (once or after code changes)
make tt list      # Run CLI command with pre-built image
make tt projects
make tt fill --from 2025-12-01 --to 2025-12-15
```

### Dev Mode (hot reload)
```bash
make start        # Start dev environment with hot reload
make tt list      # Runs via docker exec (picks up code changes automatically)
make stop         # Stop dev container
make logs         # View container logs
```

### Other
```bash
make auth         # Refresh DevPro token via browser (runs on host)
make clean        # Remove containers, volumes, and images
```

**Note:** `make tt` auto-detects mode - uses dev container if running, otherwise uses pre-built image.

## CLI Commands

- `tt fill` - Main command: sync Chrono entries to DevPro (interactive day-by-day mode)
- `tt fill --from YYYY-MM-DD --to YYYY-MM-DD` - Batch mode for date range

Note: Use `make auth` to refresh token (runs on host with browser, not in Docker).

## Architecture

### Data Flow (fill command)
1. Fetch time entries from Chrono API
2. Aggregate entries by date+project using config mappings
3. Normalize hours to 8h/day (proportional scaling, meetings preserved)
4. Generate task titles from Chrono descriptions
5. Match with existing DevPro worklogs (create or update)
6. Interactive review and apply

### Key Components

- **API Clients** (`api/`): TtApiClient (DevPro), ChronoClient
- **Config** (`config/`): YAML config loader for `~/.tt-config.yaml`
- **Commands** (`commands/`): Clikt-based CLI commands
- **Services** (`service/`): Aggregator (groups entries by date+project), TimeNormalizer (8h/day normalization)

### Configuration

Authentication: `~/.tt-token` or `TT_TOKEN` env var

Config file `~/.tt-config.yaml`:
```yaml
chrono_api: "http://localhost:9247"

mappings:
  - chrono_project: "Your Chrono Project"
    devpro_project: "DevPro Project Name"
    billability: "Billable"
```

### Tech Stack

- Kotlin 1.9, JDK 17
- Clikt (CLI framework)
- Ktor Client (HTTP)
- kotlinx.serialization + kaml (JSON/YAML)
