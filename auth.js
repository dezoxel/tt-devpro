#!/usr/bin/env node
// auth.js - DevPro session refresh via Firefox with persistent session
// After first login, subsequent runs skip login/password/captcha/MFA
//
// The portal authenticates API calls with a server-side session cookie scoped
// to .dev.pro (it no longer accepts the Firebase Bearer token from IndexedDB).
// We extract the full `name=value` pair and verbatim-store it, so a future
// cookie-name change needs no code change.
//
// A cookie's presence proves nothing: the persistent profile keeps a dead
// cookie forever, and Playwright hands back cookies past their expiry. Success
// here means the portal answered 200 to the exact cookie we are about to write.

const { firefox } = require('playwright');
const fs = require('fs');
const path = require('path');

const COOKIE_FILE = path.join(process.env.HOME, '.tt-cookie');
const USER_DATA_DIR = path.join(process.env.HOME, '.tt-browser-profile');
const PORTAL_URL = 'https://timetrackingportal.dev.pro/';
const VERIFY_URL = 'https://timetrackingportal.dev.pro/api/contact/currentUser';
const COOKIE_DOMAIN = 'dev.pro';
const TIMEOUT_MS = Number(process.env.TT_AUTH_TIMEOUT_MS) || 120000; // 2 min for first login

// Asks the portal whether this exact cookie string is a live session. Returns
// the current user on 200, null on rejection. Deliberately bypasses the browser
// jar: what gets verified is byte-for-byte what tt-devpro will send. Redirects
// are not followed, so a login page can never pose as a 200.
async function verifySession(cookie) {
    const response = await fetch(VERIFY_URL, {
        headers: { Cookie: cookie },
        redirect: 'manual',
    });
    if (response.status !== 200) return null;
    try {
        return await response.json();
    } catch {
        return null;
    }
}

// Returns the portal session cookie as `name=value`, or null if not present yet.
// context.cookies(url) returns httpOnly cookies too (unlike document.cookie) and
// is already scoped to cookies valid for the portal URL — but not to cookies
// that are still alive, hence the expiry filter (expires === -1 means the cookie
// dies with the browser session, not that it is stale).
async function extractSessionCookie(context) {
    const cookies = await context.cookies(PORTAL_URL);
    const alive = (c) => c.expires === -1 || c.expires * 1000 > Date.now();
    const session = cookies.find((c) => c.domain.includes(COOKIE_DOMAIN) && c.value && alive(c));
    return session ? `${session.name}=${session.value}` : null;
}

// The portal never bounces to Google on its own: /login sits and waits for a
// click on its single "Login to Account" button. With a live Google session in
// the profile that click completes the OAuth round-trip on its own; otherwise
// the visible window lets the login be finished by hand, so a missing button is
// a nudge, not a failure.
async function startLogin(page) {
    try {
        await page.getByRole('button', { name: /login to account/i }).click({ timeout: 10000 });
    } catch {
        console.log('⚠️  Login button not found — please finish the login in the browser window.');
    }
}

// Polls until the portal accepts a cookie from the browser jar, and returns
// `{ cookie, user }`. Callers clear the .dev.pro cookies first, so anything
// found here is a freshly issued session by construction. A cookie the portal
// already rejected is not re-checked — the portal may hand out an anonymous
// cookie long before login completes.
async function waitForLiveSession(context, timeoutMs) {
    const start = Date.now();
    let announced = false;
    let rejected = null;

    while (Date.now() - start < timeoutMs) {
        const cookie = await extractSessionCookie(context);
        if (cookie && cookie !== rejected) {
            const user = await verifySession(cookie);
            if (user) return { cookie, user };
            rejected = cookie;
        }
        if (!announced) {
            console.log('⏳ Waiting for login...');
            announced = true;
        }
        await new Promise((resolve) => setTimeout(resolve, 1000));
    }
    throw new Error('Timed out waiting for a session the portal accepts');
}

function saveCookie(cookie, user) {
    fs.writeFileSync(COOKIE_FILE, cookie);
    console.log(`✓ Verified session for ${user.fullName} <${user.email}>`);
    console.log(`✓ Session cookie saved to ${COOKIE_FILE}`);
}

async function main() {
    console.log('🔐 Dev.Pro Time Tracking Portal Authentication');
    console.log('==============================================\n');

    // Use persistent context - saves session between runs
    const context = await firefox.launchPersistentContext(USER_DATA_DIR, {
        headless: false,
    });

    let failed = false;

    try {
        // Inside the try: a failure here must still reach the close() below,
        // or a half-launched Firefox is left on screen with nothing to close it.
        const page = context.pages()[0] || await context.newPage();
        await page.goto(PORTAL_URL);

        const saved = await extractSessionCookie(context);
        const savedUser = saved ? await verifySession(saved) : null;
        if (savedUser) {
            saveCookie(saved, savedUser);
        } else {
            console.log(
                saved
                    ? '↻ Saved session rejected by the portal — clearing it and logging in again.'
                    : '↻ No live session in the browser profile — logging in.'
            );
            // Drop only .dev.pro cookies: the Google/MFA session lives on other
            // domains and is what keeps re-login a silent round-trip. Without
            // this, the dead cookie would be harvested again on the next poll.
            await context.clearCookies({ domain: /dev\.pro$/ });
            await page.goto(PORTAL_URL);
            await startLogin(page);

            const { cookie, user } = await waitForLiveSession(context, TIMEOUT_MS);
            saveCookie(cookie, user);
        }
    } catch (error) {
        console.error('❌ Authentication failed:', error.message);
        console.error(`   ${COOKIE_FILE} left untouched — the portal accepted no session.`);
        failed = true;
    } finally {
        // Close before exiting: process.exit() skips the rest of finally and
        // would strand the Firefox window.
        await context.close();
    }

    if (failed) process.exit(1);

    console.log('\n✅ Done!');
}

main();
