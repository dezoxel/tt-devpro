#!/bin/bash
# auth.sh - Refresh DevPro session cookie via browser
#
# This script runs the authentication flow on the host machine (not in Docker)
# because Playwright cannot open a GUI browser inside Docker containers on macOS.
#
# The session cookie will be saved to ~/.tt-cookie and will be available to the
# Docker container via volume mount.

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Check if node_modules exists, if not install dependencies
if [ ! -d "node_modules/playwright" ]; then
    echo "📦 Installing Playwright..."
    npm install playwright
    npx playwright install firefox
    echo ""
fi

# Run the auth script
node auth.js
