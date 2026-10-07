#!/bin/bash
# auth.sh - Refresh DevPro session cookie via browser
#
# The login cannot be done headlessly, which is why authentication is a separate
# host-side script rather than something the tt-devpro binary does itself: auth.js
# launches Firefox with `headless: false` and waits for the portal's Google OAuth
# round-trip, MFA included, to be completed by hand in a visible window. It needs
# a desktop session with a display; the binary never opens one.
#
# On success the cookie the portal accepted is written into the 1Password item
# that `session_cookie` in ~/.tt-config.yaml refers to, which is exactly where
# the binary reads it from (src/cookie.rs).

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Install the Playwright version package.json pins, and the Firefox build it
# drives (into ~/.cache/ms-playwright), the first time only.
if [ ! -d "node_modules/playwright" ]; then
    echo "📦 Installing Playwright..."
    npm install
    npx playwright install firefox
    echo ""
fi

# Run the auth script
node auth.js
