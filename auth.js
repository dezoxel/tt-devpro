#!/usr/bin/env node
// auth.js - DevPro session refresh: browser login, cookie into 1Password
//
// The portal authenticates API calls with a server-side session cookie scoped
// to .dev.pro (it no longer accepts the Firebase Bearer token from IndexedDB).
// We extract the full `name=value` pair and verbatim-store it, so a future
// cookie-name change needs no code change.
//
// The browser starts from an empty profile every time, so every run is a full
// login with MFA. A profile kept between runs would hold a live Google session
// on disk in plaintext, which is the thing the cookie moved to 1Password to
// avoid; the login is needed about once in two weeks, and that is the price.
//
// The cookie is stored in the 1Password item that `session_cookie` in
// ~/.config/tt-devpro/config.yaml refers to, and the tt-devpro binary reads it from there.
// It never touches the disk and never appears in a command line: `op` receives
// it as item JSON on stdin.
//
// A cookie's presence proves nothing: Playwright hands back cookies past their
// expiry, and the portal may issue an anonymous cookie before the login is
// done. Success here means the portal answered 200 to the exact cookie stored,
// and 1Password gave the same cookie back when asked.

const { firefox } = require('playwright');
const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');

// Playwright resolves its browser by revision number, and that number is not
// ours to control: playwright-core 1.62 wants firefox-1538, the globally
// installed @playwright/cli wants firefox-1544, and in the alpha channel the
// revision moves almost every day. Every mismatch costs a fresh ~280 MB download
// and leaves another "Nightly" in the macOS default-browser list — on 20 September
// 2026 there were two builds in the cache and nine dead entries in LaunchServices
// from the ones before them.
//
// What this script asks of Firefox — open the portal, let the login happen in a
// visible window, read one cookie — is the same in any recent build, so the build
// already in the cache is as good as the one our pinned version names. Pinning the
// version stays worthwhile for reproducibility; this only stops the pin from
// forcing a second download.
function browsersRoot() {
    if (process.env.PLAYWRIGHT_BROWSERS_PATH) return process.env.PLAYWRIGHT_BROWSERS_PATH;
    if (process.platform === 'darwin') return path.join(process.env.HOME, 'Library', 'Caches', 'ms-playwright');
    if (process.platform === 'win32') return path.join(process.env.LOCALAPPDATA || '', 'ms-playwright');
    return path.join(process.env.HOME, '.cache', 'ms-playwright');
}

const FIREFOX_BINARY = {
    darwin: path.join('firefox', 'Nightly.app', 'Contents', 'MacOS', 'firefox'),
    win32: path.join('firefox', 'firefox.exe'),
}[process.platform] || path.join('firefox', 'firefox');

// Returns a path to override Playwright's own resolution with, or undefined to
// leave it alone. Undefined is the answer in both good cases: the pinned revision
// is present, or the cache is empty and Playwright's own "run npx playwright
// install" error is the right thing for the user to read.
function cachedFirefoxPath() {
    let pinned = null;
    try {
        pinned = firefox.executablePath();
    } catch {
        // Playwright could not name a path at all; the cache scan below still can.
    }
    if (pinned && fs.existsSync(pinned)) return undefined;

    let entries;
    try {
        entries = fs.readdirSync(browsersRoot());
    } catch {
        return undefined;
    }

    const newest = entries
        .map((name) => /^firefox-(\d+)$/.exec(name))
        .filter(Boolean)
        .map((match) => ({ revision: Number(match[1]), binary: path.join(browsersRoot(), match[0], FIREFOX_BINARY) }))
        .filter((build) => fs.existsSync(build.binary))
        .sort((a, b) => b.revision - a.revision)[0];

    if (!newest) return undefined;
    console.log(`↻ Reusing the Firefox already in the Playwright cache (revision ${newest.revision}).`);
    return newest.binary;
}

const CONFIG_FILE = path.join(process.env.HOME, '.config', 'tt-devpro', 'config.yaml');
const PORTAL_URL = 'https://timetrackingportal.dev.pro/';
const VERIFY_URL = 'https://timetrackingportal.dev.pro/api/contact/currentUser';
const COOKIE_DOMAIN = 'dev.pro';
// Every login is a full one, MFA included, so the wait is sized for that.
const TIMEOUT_MS = Number(process.env.TT_AUTH_TIMEOUT_MS) || 300000;
// The category `make auth` creates the item in when it does not exist yet.
const ITEM_CATEGORY = 'API Credential';

// The `session_cookie` reference from ~/.config/tt-devpro/config.yaml, split into the vault,
// the item and the field. A regex rather than a YAML parser: this is the one key
// the script needs, and the binary validates the whole file on every run. The
// reference must be `op://vault/item/field`; a section in between is not
// supported, so it fails here rather than writing to the wrong field.
function sessionCookieReference() {
    let config;
    try {
        config = fs.readFileSync(CONFIG_FILE, 'utf8');
    } catch (error) {
        throw new Error(`cannot read ${CONFIG_FILE}: ${error.message}`);
    }
    const match = /^session_cookie:\s*["']?(op:\/\/[^"'\n]+?)["']?\s*$/m.exec(config);
    if (!match) {
        throw new Error(`no session_cookie: "op://vault/item/field" line in ${CONFIG_FILE}`);
    }
    const reference = match[1];
    const parts = reference.slice('op://'.length).split('/');
    if (parts.length !== 3 || parts.some((part) => part === '')) {
        throw new Error(`session_cookie must be op://vault/item/field, got ${reference}`);
    }
    const [vault, item, field] = parts;
    return { reference, vault, item, field };
}

// Runs `op` with the given stdin and returns its stdout. Each call asks for
// approval in 1Password. stdout is returned, never printed: for `read` and
// `item get` it holds the cookie.
//
// With stdin, `op` runs behind `cat |` in sh. Node hands a child its stdin as a
// socket, and `op` reads a JSON item from stdin only when stdin is a pipe: given
// the socket, `item create` failed asking for `--category`. The cookie still
// travels on stdin only, never in argv or on disk.
function op(args, input) {
    const result = input === undefined
        ? spawnSync('op', args, { encoding: 'utf8' })
        : spawnSync('sh', ['-c', 'cat | op "$@"', 'sh', ...args], { input, encoding: 'utf8' });
    if (result.error) throw new Error(`cannot run op (the 1Password CLI): ${result.error.message}`);
    if (result.status !== 0) {
        const error = new Error(`op ${args.slice(0, 2).join(' ')} failed: ${result.stderr.trim()}`);
        error.stderr = result.stderr;
        throw error;
    }
    return result.stdout;
}

// The item the cookie goes into, as `op item get --format json` returns it, or
// null when the vault has no such item yet. Asked before the browser opens, so
// a locked 1Password or a wrong vault shows up before the login, not after it.
function existingItem(ref) {
    try {
        return JSON.parse(op(['item', 'get', ref.item, '--vault', ref.vault, '--format', 'json']));
    } catch (error) {
        if (error.stderr && /isn't an item/.test(error.stderr)) return null;
        throw error;
    }
}

// Sets the field named by the reference, matched by id or by label the way
// `op read` matches it. The value goes in as JSON, so it never reaches argv.
function withCookie(item, ref, cookie) {
    const field = (item.fields || []).find((f) => f.id === ref.field || f.label === ref.field);
    if (!field) {
        const names = (item.fields || []).map((f) => f.label || f.id).join(', ');
        throw new Error(`the item has no field "${ref.field}" (it has: ${names})`);
    }
    field.value = cookie;
    return JSON.stringify(item);
}

// Writes the cookie into 1Password, then reads it back through the reference
// itself: `op` has been seen to drop a field from a JSON edit without an error,
// so the write counts only when the read returns the same cookie.
function storeCookie(ref, item, cookie) {
    if (item) {
        op(['item', 'edit', item.id, '--vault', ref.vault], withCookie(item, ref, cookie));
    } else {
        const template = JSON.parse(op(['item', 'template', 'get', ITEM_CATEGORY, '--format', 'json']));
        template.title = ref.item;
        op(['item', 'create', '--vault', ref.vault, '-'], withCookie(template, ref, cookie));
    }
    if (op(['read', '--no-newline', ref.reference]) !== cookie) {
        throw new Error(`1Password returned a different value for ${ref.reference} than was written`);
    }
}

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
// click on its single "Login to Account" button. The click opens the Google
// login, which is finished by hand in the visible window, so a missing button
// is a nudge, not a failure.
async function startLogin(page) {
    try {
        await page.getByRole('button', { name: /login to account/i }).click({ timeout: 10000 });
    } catch {
        console.log('⚠️  Login button not found — please finish the login in the browser window.');
    }
}

// Polls until the portal accepts a cookie from the browser jar, and returns
// `{ cookie, user }`. The context starts empty, so anything found here is a
// freshly issued session by construction. A cookie the portal already rejected
// is not re-checked — the portal may hand out an anonymous cookie long before
// login completes.
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

async function main() {
    console.log('🔐 Dev.Pro Time Tracking Portal Authentication');
    console.log('==============================================\n');

    let ref;
    let item;
    try {
        ref = sessionCookieReference();
        item = existingItem(ref);
    } catch (error) {
        console.error('❌ Authentication not started:', error.message);
        process.exit(1);
    }
    console.log(item ? `↻ Will update ${ref.reference}` : `+ Will create ${ref.reference}`);

    const executablePath = cachedFirefoxPath();
    const browser = await firefox.launch({
        headless: false,
        ...(executablePath ? { executablePath } : {}),
    });

    let failed = false;

    try {
        // Inside the try: a failure here must still reach the close() below,
        // or a half-launched Firefox is left on screen with nothing to close it.
        const context = await browser.newContext();
        const page = await context.newPage();
        await page.goto(PORTAL_URL);
        await startLogin(page);

        const { cookie, user } = await waitForLiveSession(context, TIMEOUT_MS);
        console.log(`✓ Verified session for ${user.fullName} <${user.email}>`);
        storeCookie(ref, item, cookie);
        console.log(`✓ Session cookie stored in ${ref.reference}`);
    } catch (error) {
        console.error('❌ Authentication failed:', error.message);
        failed = true;
    } finally {
        // Close before exiting: process.exit() skips the rest of finally and
        // would strand the Firefox window.
        await browser.close();
    }

    if (failed) process.exit(1);

    console.log('\n✅ Done!');
}

// Running the file logs in; requiring it exposes the browser resolution so it
// can be exercised without opening a window.
if (require.main === module) main();

module.exports = { cachedFirefoxPath };
