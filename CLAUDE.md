# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

tt-devpro is a Kotlin CLI tool for synchronizing time entries from Chrono (local time tracking) to DevPro Time Tracking Portal. It uses Ollama LLM to generate task titles from work descriptions.

## Build & Run Commands

**IMPORTANT: Always run via Docker, never use `./gradlew run` directly.**

```bash
# Start Docker container with hot reload
./dev.sh start

# Execute CLI commands inside Docker
./dev.sh tt fill --from 2025-12-01 --to 2025-12-15
./dev.sh tt list
./dev.sh tt projects

# Other dev.sh commands
./dev.sh stop     # Stop container
./dev.sh logs     # View logs
./dev.sh clean    # Clean including Gradle cache

# Build only (for CI or testing)
./gradlew build
./gradlew test
```

## CLI Commands

- `tt fill` - Main command: sync Chrono entries to DevPro (interactive day-by-day mode)
- `tt fill --from YYYY-MM-DD --to YYYY-MM-DD` - Batch mode for date range
- `tt auth` - Refresh DevPro token via browser (run `./auth.sh` on host if in Docker)
- `tt list [-p YYYY-MM-DD]` - List worklogs for a period
- `tt projects` - List available DevPro projects
- `tt create/update/delete` - CRUD operations for worklogs

## Architecture

### Data Flow (fill command)
1. Fetch time entries from Chrono API
2. Aggregate entries by date+project using config mappings
3. Generate task titles via Ollama LLM (bypassed for Operations/Meetings)
4. Match with existing DevPro worklogs (create or update)
5. Interactive review and apply

### Key Components

- **API Clients** (`api/`): TtApiClient (DevPro), ChronoClient, OllamaClient
- **Config** (`config/`): YAML config loader for `~/.tt-config.yaml`
- **Commands** (`commands/`): Clikt-based CLI commands
- **Aggregator** (`service/`): Groups Chrono entries by date+project, resolves project mappings

### Configuration

Authentication: `~/.tt-token` or `TT_TOKEN` env var

Config file `~/.tt-config.yaml`:
```yaml
chrono_api: "http://localhost:9247"
ollama_api: "http://localhost:11434"
ollama_model: "llama3.2"

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
