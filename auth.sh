#!/bin/bash
# auth.sh - Refresh DevPro session cookie via browser
#
# The login cannot be done headlessly, which is why authentication is a separate
# host-side script rather than something the tt-devpro binary does itself: auth.js
# launches Firefox with `headless: false` and waits for the portal's Google OAuth
# round-trip — including MFA the first time — to be completed by hand in a visible
# window. It needs a desktop session with a display; the binary never opens one.
#
# On success the cookie the portal accepted is written to ~/.tt-cookie, which is
# exactly where the binary reads it from (src/cookie.rs).

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
