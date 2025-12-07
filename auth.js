#!/usr/bin/env node
// auth.js - DevPro token refresh via Firefox with persistent session
// After first login, subsequent runs skip login/password/captcha/MFA

const { firefox } = require('playwright');
const fs = require('fs');
const path = require('path');

const TOKEN_FILE = path.join(process.env.HOME, '.tt-token');
const USER_DATA_DIR = path.join(process.env.HOME, '.tt-browser-profile');
const PORTAL_URL = 'https://timetrackingportal.dev.pro/';
const TIMEOUT_MS = 120000; // 2 minutes for first login

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
    console.log('🔐 Dev.Pro Time Tracking Portal Authentication');
    console.log('==============================================\n');

    // Use persistent context - saves session between runs
    const context = await firefox.launchPersistentContext(USER_DATA_DIR, {
        headless: false,
    });

    const page = context.pages()[0] || await context.newPage();

    try {
        await page.goto(PORTAL_URL);

        // Check if already logged in
        const currentUrl = page.url();
        if (currentUrl.includes('/time-tracking')) {
            console.log('✓ Already logged in! Extracting token...');
        } else {
            console.log('⏳ Waiting for login (first time only)...');
            await page.waitForURL('**/time-tracking**', { timeout: TIMEOUT_MS });
            console.log('✓ Login successful!');
        }

        await page.waitForTimeout(1000);
        const token = await extractToken(page);

        fs.writeFileSync(TOKEN_FILE, token);
        console.log(`✓ Token saved to ${TOKEN_FILE}`);

    } catch (error) {
        console.error('❌ Authentication failed:', error.message);
        process.exit(1);
    } finally {
        await context.close();
    }

    console.log('\n✅ Done!');
}

main();
