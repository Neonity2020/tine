package page.tine.app

import android.content.Context
import android.os.Environment
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import java.io.File
import java.util.UUID
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith

object ManagedStorageSmoke {
  external fun runManagedActivationSmoke(graphRoot: String, privateRoot: String): String
}

@RunWith(AndroidJUnit4::class)
class ManagedStorageSmokeTest {
  @Test
  fun activationShareSetupCleanShutdownAndReopenWorkAsTheAppUidOnSharedStorage() {
    val context = ApplicationProvider.getApplicationContext<Context>()
    val nonce = UUID.randomUUID().toString()
    val graphRoot = File(
      Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS),
      "tine-managed-storage-smoke-$nonce",
    )
    val privateRoot = File(context.filesDir, "managed-storage-smoke/$nonce")

    graphRoot.deleteRecursively()
    privateRoot.deleteRecursively()
    File(graphRoot, "pages").mkdirs()
    File(graphRoot, "journals").mkdirs()
    File(graphRoot, "logseq").mkdirs()
    File(graphRoot, "pages/Smoke.md").writeText("- Android managed storage smoke\n")
    File(graphRoot, "logseq/config.edn").writeText("{}\n")

    System.loadLibrary("tine_lib")
    try {
      val result = ManagedStorageSmoke.runManagedActivationSmoke(
        graphRoot.absolutePath,
        privateRoot.absolutePath,
      )
      assertEquals("ok", result)
      assertEquals(
        "- Android managed storage smoke\n",
        File(graphRoot, "pages/Smoke.md").readText(),
      )
    } finally {
      graphRoot.deleteRecursively()
      privateRoot.deleteRecursively()
    }
  }

  @Test
  fun activationRebuildsAnInterruptedPrePromotionReceiptTree() {
    val context = ApplicationProvider.getApplicationContext<Context>()
    val nonce = UUID.randomUUID().toString()
    val graphRoot = File(
      Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS),
      "tine-managed-storage-resume-$nonce",
    )
    val privateRoot = File(context.filesDir, "managed-storage-resume/$nonce")

    graphRoot.deleteRecursively()
    privateRoot.deleteRecursively()
    File(graphRoot, "pages").mkdirs()
    File(graphRoot, "journals").mkdirs()
    File(graphRoot, "logseq").mkdirs()
    File(graphRoot, "pages/Resume.md").writeText("- Android interrupted activation resume\n")
    File(graphRoot, "logseq/config.edn").writeText("{}\n")
    // This is deliberately not a valid receipt store. It represents bytes
    // left by a killed, pre-promotion candidate; the Markdown graph is still
    // the sole authority and retry must rebuild disposable private state.
    File(privateRoot, "receipts").mkdirs()
    File(privateRoot, "receipts/interrupted.tmp").writeText("partial\n")

    System.loadLibrary("tine_lib")
    try {
      val result = ManagedStorageSmoke.runManagedActivationSmoke(
        graphRoot.absolutePath,
        privateRoot.absolutePath,
      )
      assertEquals("ok", result)
      assertEquals(
        "- Android interrupted activation resume\n",
        File(graphRoot, "pages/Resume.md").readText(),
      )
      assertEquals(
        "partial\n",
        File(privateRoot, "receipts.pre-promotion-failed/interrupted.tmp").readText(),
      )
    } finally {
      graphRoot.deleteRecursively()
      privateRoot.deleteRecursively()
    }
  }
}
