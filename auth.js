#!/usr/bin/env node
// auth.js - DevPro session refresh via Firefox with persistent session
// After first login, subsequent runs skip login/password/captcha/MFA
//
// The portal authenticates API calls with a server-side session cookie scoped
// to .dev.pro (it no longer accepts the Firebase Bearer token from IndexedDB).
// We extract the full `name=value` pair and verbatim-store it, so a future
// cookie-name change needs no code change.

const { firefox } = require('playwright');
const fs = require('fs');
const path = require('path');

const COOKIE_FILE = path.join(process.env.HOME, '.tt-cookie');
const USER_DATA_DIR = path.join(process.env.HOME, '.tt-browser-profile');
const PORTAL_URL = 'https://timetrackingportal.dev.pro/';
const COOKIE_DOMAIN = 'dev.pro';
const TIMEOUT_MS = 120000; // 2 minutes for first login

// Returns the portal session cookie as `name=value`, or null if not present yet.
// context.cookies(url) returns httpOnly cookies too (unlike document.cookie) and
// is already scoped to cookies valid for the portal URL.
async function extractSessionCookie(context) {
    const cookies = await context.cookies(PORTAL_URL);
    const session = cookies.find((c) => c.domain.includes(COOKIE_DOMAIN) && c.value);
    return session ? `${session.name}=${session.value}` : null;
}

// Poll until the session cookie is set: appears immediately for an already
// logged-in persistent session, or once the user completes login on first run.
// Decoupled from any redirect URL (the post-login target is brittle).
async function waitForSessionCookie(context, timeoutMs) {
    const start = Date.now();
    let announced = false;
    while (Date.now() - start < timeoutMs) {
        const cookie = await extractSessionCookie(context);
        if (cookie) return cookie;
        if (!announced) {
            console.log('⏳ Waiting for login (first time only)...');
            announced = true;
        }
        await new Promise((resolve) => setTimeout(resolve, 1000));
    }
    throw new Error('Timed out waiting for session cookie');
}

async function main() {
    console.log('🔐 Dev.Pro Time Tracking Portal Authentication');
    console.log('==============================================\n');

    // Use persistent context - saves session between runs
    const context = await firefox.launchPersistentContext(USER_DATA_DIR, {
        headless: false,
    });

    const page = context.pages()[0] || await context.newPage();

    try {
        await page.goto(PORTAL_URL);

        const cookie = await waitForSessionCookie(context, TIMEOUT_MS);
        console.log('✓ Login successful!');

        fs.writeFileSync(COOKIE_FILE, cookie);
        console.log(`✓ Session cookie saved to ${COOKIE_FILE}`);

    } catch (error) {
        console.error('❌ Authentication failed:', error.message);
        process.exit(1);
    } finally {
        await context.close();
    }

    console.log('\n✅ Done!');
}

main();
