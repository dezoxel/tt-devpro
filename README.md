# tt-devpro

CLI tool for syncing time entries from Chrono to DevPro Time Tracking Portal.

## Quick Start

```bash
# Start dev environment
make start

# Run CLI commands
make tt list
make tt projects
make tt fill --from 2025-12-01 --to 2025-12-15

# View logs
make logs

# Stop
make stop
```

## Commands

Run `make help` for all available commands:

```
Docker:
  make start      Start dev environment with hot reload
  make stop       Stop dev environment
  make restart    Restart dev environment
  make logs       Show container logs
  make clean      Stop and remove volumes (Gradle cache)

Build:
  make build      Build the application

CLI (runs inside Docker):
  make tt CMD     Run tt command (e.g., make tt list)

Auth:
  make auth       Refresh DevPro token via browser (runs on host)
```

## Authentication

Before using the CLI, you need to authenticate with DevPro Time Tracking Portal.

### Token refresh

Run on your **host machine** (not in Docker):

```bash
make auth
```

This will:
1. Open a browser window
2. Navigate to DevPro Time Tracking Portal
3. Wait for you to complete Google OAuth login
4. Extract and save the authentication token to `~/.tt-token`

The token is automatically mounted into the Docker container.

### Why run on host?

Playwright cannot open a GUI browser inside Docker containers on macOS. The `make auth` command runs the authentication flow on your host machine where the browser can be displayed.
