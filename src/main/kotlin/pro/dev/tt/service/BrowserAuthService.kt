package pro.dev.tt.service

import com.microsoft.playwright.Browser
import com.microsoft.playwright.BrowserType
import com.microsoft.playwright.Page
import com.microsoft.playwright.Playwright
import java.io.File

class BrowserAuthService {
    companion object {
        private const val DEVPRO_URL = "https://timetrackingportal.dev.pro/"
        private const val TIMEOUT_MS = 120_000.0 // 2 minutes for user to login

        /**
         * Opens browser for user to login and extracts the Firebase auth token from IndexedDB.
         * Returns the token string or null if failed.
         */
        fun refreshTokenViaBrowser(): String? {
            println("Opening browser for authentication...")
            println("Please log in with your Google account.")

            return try {
                Playwright.create().use { playwright ->
                    val browser = playwright.chromium().launch(
                        BrowserType.LaunchOptions().setHeadless(false)
                    )

                    browser.use {
                        val context = browser.newContext()
                        val page = context.newPage()

                        // Navigate to DevPro
                        page.navigate(DEVPRO_URL)

                        // Wait for user to complete login
                        // Login is complete when URL changes to /time-tracking
                        println("Waiting for login to complete...")
                        page.waitForURL("**/time-tracking**", Page.WaitForURLOptions().setTimeout(TIMEOUT_MS))

                        println("Login detected! Extracting token...")

                        // Extract token from IndexedDB
                        val token = extractTokenFromIndexedDB(page)

                        if (token != null) {
                            println("Token successfully extracted!")
                        } else {
                            println("Failed to extract token from IndexedDB")
                        }

                        token
                    }
                }
            } catch (e: Exception) {
                println("Browser authentication failed: ${e.message}")
                null
            }
        }

        /**
         * Saves the token to ~/.tt-token file
         */
        fun saveToken(token: String): Boolean {
            return try {
                val tokenFile = File(System.getProperty("user.home"), ".tt-token")
                tokenFile.writeText(token)
                println("Token saved to ${tokenFile.absolutePath}")
                true
            } catch (e: Exception) {
                println("Failed to save token: ${e.message}")
                false
            }
        }

        private fun extractTokenFromIndexedDB(page: Page): String? {
            return try {
                val result = page.evaluate("""
                    () => {
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
                              reject('Token not found');
                            }
                          };
                          getAllRequest.onerror = () => reject('Failed to get data');
                        };
                      });
                    }
                """.trimIndent())

                result?.toString()
            } catch (e: Exception) {
                println("Failed to extract token from IndexedDB: ${e.message}")
                null
            }
        }
    }
}
