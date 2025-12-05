# tt-devpro

## Authentication

Before using the CLI, you need to authenticate with DevPro Time Tracking Portal.

### First-time setup or token refresh

Run the authentication script on your **host machine** (not in Docker):

```bash
./auth.sh
```

This will:
1. Open a browser window
2. Navigate to DevPro Time Tracking Portal
3. Wait for you to complete Google OAuth login
4. Extract and save the authentication token to `~/.tt-token`

The token is automatically mounted into the Docker container and will be used for all API requests.

### Manual authentication (alternative)

You can also run the auth command directly:

```bash
./gradlew run --args="auth"
```

### Why run on host?

Playwright cannot open a GUI browser inside Docker containers on macOS. The `auth.sh` script runs the authentication flow on your host machine where the browser can be displayed.
