#!/usr/bin/env node
// auth.js - DevPro token refresh via Playwright
// Usage: npx playwright install firefox && node auth.js

const { firefox } = require('playwright');
const fs = require('fs');
const path = require('path');

const TOKEN_FILE = path.join(process.env.HOME, '.tt-token');
const PORTAL_URL = 'https://timetrackingportal.dev.pro/';
const TIMEOUT_MS = 120000; // 2 minutes for login

async function extractToken(page) {
    return await page.evaluate(() => {
        return new Promise((resolve, reject) => {
            const request = indexedDB.open('firebaseLocalStorageDb');
            request.onerror = () => reject('Failed to open IndexedDB');
            request.onsuccess = (event) => {
                const db = event.target.result;
                const transaction = db.transaction(['firebaseLocalStorage'], 'readonly');
                const store = transaction.objectStore('firebaseLocalStorage');
                const getAllRequest = store.getAll();
                getAllRequest.onsuccess = () => {
                    const data = getAllRequest.result[0];
                    if (data && data.value && data.value.stsTokenManager) {
                        resolve(data.value.stsTokenManager.accessToken);
                    } else {
                        reject('Token not found in IndexedDB');
                    }
                };
                getAllRequest.onerror = () => reject('Failed to get data from IndexedDB');
            };
        });
    });
}

async function main() {
    console.log('🔐 DevPro Authentication');
    console.log('========================\n');
    console.log('Opening browser for Google OAuth login...\n');

    const browser = await firefox.launch({ headless: false });
    const context = await browser.newContext();
    const page = await context.newPage();

    try {
        await page.goto(PORTAL_URL);

        // Wait for successful login (URL changes to /time-tracking)
        console.log('⏳ Waiting for login (timeout: 2 minutes)...');
        await page.waitForURL('**/time-tracking**', { timeout: TIMEOUT_MS });

        console.log('✓ Login successful! Extracting token...');

        // Small delay to ensure IndexedDB is populated
        await page.waitForTimeout(1000);

        const token = await extractToken(page);

        // Save token to file
        fs.writeFileSync(TOKEN_FILE, token);
        console.log(`✓ Token saved to ${TOKEN_FILE}`);

    } catch (error) {
        console.error('❌ Authentication failed:', error.message);
        process.exit(1);
    } finally {
        await browser.close();
    }

    console.log('\n✅ Done! You can now use tt commands in Docker.');
}

main();
