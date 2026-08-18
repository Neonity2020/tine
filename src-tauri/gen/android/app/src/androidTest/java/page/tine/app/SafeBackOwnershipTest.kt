package page.tine.app

import android.os.SystemClock
import android.webkit.WebView
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Who owns the hardware Back gesture once Tauri has finished starting.
 *
 * OnBackPressedDispatcher hands a gesture to the most recently added enabled
 * callback. Tauri's core AppPlugin adds one from its constructor, which runs on
 * the Rust startup thread — after MainActivity.onCreate has returned. Whenever
 * the WebView has history, AppPlugin's no-listener branch answers Back with
 * WebView.goBack() and Tine's SafeBack owner is never consulted: on screen, a
 * route quietly changes behind an open modal and the modal stays open.
 *
 * The WebView history is the part that makes this observable. Without it
 * AppPlugin disables itself and re-dispatches, so Back reaches Tine's owner
 * even when Tine does not own it — which is exactly why the defect survived.
 */
@RunWith(AndroidJUnit4::class)
class SafeBackOwnershipTest {
  @Test
  fun tineOwnsBackAfterTauriStartupEvenWhenTheWebViewHasHistory() {
    // Deliberately try/finally rather than `use`: ActivityScenario is a Java
    // AutoCloseable, and this module's Kotlin settings are not something a test
    // should depend on.
    val scenario = ActivityScenario.launch(MainActivity::class.java)
    try {
      val webView = awaitWebView()

      // Reproduce what the mobile router does on every navigation
      // (pushMobileHistoryEntry in src/router.ts): one real WebView history
      // entry. Anything less and AppPlugin's fallback re-dispatches, hiding
      // the ownership question this test exists to ask.
      evaluateOnUiThread(scenario, "history.pushState(null, '', location.href)")
      assertTrue(
        "the fixture could not give the WebView a history entry, so this run " +
          "cannot observe Back ownership at all",
        awaitTrue { onUiThread(scenario) { webView.canGoBack() } },
      )

      val before = SafeBackBridge.gesturesReceived
      scenario.onActivity { it.onBackPressedDispatcher.onBackPressed() }

      assertTrue(
        "Back did not reach Tine's SafeBack owner: another OnBackPressedCallback " +
          "(Tauri's AppPlugin) was registered later and answered the gesture with " +
          "WebView history navigation instead",
        awaitTrue { SafeBackBridge.gesturesReceived > before },
      )
    } finally {
      scenario.close()
    }
  }

  private fun awaitWebView(): WebView {
    val deadline = SystemClock.elapsedRealtime() + STARTUP_TIMEOUT_MS
    while (SystemClock.elapsedRealtime() < deadline) {
      SafeBackBridge.webViewForTest()?.let { return it }
      SystemClock.sleep(POLL_MS)
    }
    throw AssertionError("Tauri never loaded SafeBackPlugin's WebView")
  }

  private fun awaitTrue(condition: () -> Boolean): Boolean {
    val deadline = SystemClock.elapsedRealtime() + STARTUP_TIMEOUT_MS
    while (SystemClock.elapsedRealtime() < deadline) {
      if (condition()) return true
      SystemClock.sleep(POLL_MS)
    }
    return false
  }

  private fun <T> onUiThread(
    scenario: ActivityScenario<MainActivity>,
    block: () -> T,
  ): T {
    var result: T? = null
    scenario.onActivity { result = block() }
    @Suppress("UNCHECKED_CAST")
    return result as T
  }

  private fun evaluateOnUiThread(scenario: ActivityScenario<MainActivity>, script: String) {
    val evaluated = CountDownLatch(1)
    scenario.onActivity {
      val webView = SafeBackBridge.webViewForTest()
      if (webView == null) evaluated.countDown()
      else webView.evaluateJavascript(script) { evaluated.countDown() }
    }
    evaluated.await(STARTUP_TIMEOUT_MS, TimeUnit.MILLISECONDS)
  }

  private companion object {
    const val STARTUP_TIMEOUT_MS = 60_000L
    const val POLL_MS = 100L
  }
}
