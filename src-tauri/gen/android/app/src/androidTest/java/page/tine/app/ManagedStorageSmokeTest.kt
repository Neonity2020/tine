package page.tine.app

import android.content.Context
import android.os.Environment
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import java.io.File
import java.util.UUID
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

object ManagedStorageSmoke {
  /**
   * Runs the whole managed-storage journey natively.
   *
   * When [writeFixture] is true the native side also WRITES the graph tree, from
   * the one fixture `tine_core::managed_storage_journey` shares with the host
   * test. That is deliberate: while the device fixture and the host fixture were
   * two hand-maintained copies they diverged, and the divergence was invisible —
   * this journey was green on CI in the same round a physical device flooded the
   * app with a reconciliation refusal on page-name shapes neither fixture had.
   * [returnToDirectFiles] is true for the full lifecycle journey. The separate
   * interrupted-activation test leaves the rebuilt private tree in place long
   * enough to inspect its quarantined pre-promotion receipt directly.
   */
  external fun runManagedActivationSmoke(
    graphRoot: String,
    privateRoot: String,
    writeFixture: Boolean,
    returnToDirectFiles: Boolean,
  ): String
}

@RunWith(AndroidJUnit4::class)
class ManagedStorageSmokeTest {
  @Test
  fun activationEditCrashReopenShareJoinPeerReopenAndReturnWorkAsTheAppUidOnSharedStorage() {
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
    // Match Martin's physical graph shape closely enough that per-page work,
    // shared-storage enumeration and final actor/readiness startup cannot hide
    // behind a small fixture. The journey's own page-name shapes are written
    // natively below, from the fixture the host test shares, and this corpus is
    // layered under them; neither writer removes the other's files.
    repeat(1097) { pageIndex ->
      val blocks = buildString {
        repeat(12) { blockIndex ->
          append("- Android corpus page ")
          append(pageIndex)
          append(" block ")
          append(blockIndex)
          append(" with [[Smoke]] and #android-corpus\n")
        }
      }
      File(graphRoot, "pages/Corpus-$pageIndex.md").writeText(blocks)
    }

    System.loadLibrary("tine_lib")
    try {
      // `writeFixture = true`: the native side writes the SHARED journey graph
      // (non-ASCII precomposed AND decomposed, an inline #hashtag inside a page
      // name, spaces, one title spelled two ways on disk, two names differing
      // only by case) and then drives the journey, including the external
      // reconciliation leg this test used to skip entirely.
      val result = ManagedStorageSmoke.runManagedActivationSmoke(
        graphRoot.absolutePath,
        privateRoot.absolutePath,
        true,
        true,
      )
      println("TINE_ANDROID_MANAGED_LARGE_GRAPH_RECEIPT $result")
      assertTrue(result, result.startsWith("ok "))
      assertTrue(result, result.contains("second_device_join=ok"))
      assertTrue(result, result.contains("return_to_direct=ok"))
      assertEquals(
        "- Android managed storage edited\n",
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
    File(graphRoot, "pages/Resume.md").writeText("- Android interrupted activation resume\n")
    // This is deliberately not a valid receipt store. It represents bytes
    // left by a killed, pre-promotion candidate; the Markdown graph is still
    // the sole authority and retry must rebuild disposable private state.
    File(privateRoot, "receipts").mkdirs()
    File(privateRoot, "receipts/interrupted.tmp").writeText("partial\n")

    System.loadLibrary("tine_lib")
    try {
      // `writeFixture = true` here too. This case used to hand-maintain its own
      // smaller graph, so it drove the SHARED journey against a tree the
      // journey's external-reconciliation leg does not describe: the leg's
      // "ordinary offline edit to an existing page" became a create, the
      // `archiv/` backup copy won the decoded name in the same epoch, and the
      // case failed as `external edit did not reconcile: Missing` while the
      // runtime was behaving exactly as specified (CI 32108957903). Only the
      // interrupted pre-promotion receipt tree and `Resume.md` belong to this
      // case; the graph belongs to the journey.
      val result = ManagedStorageSmoke.runManagedActivationSmoke(
        graphRoot.absolutePath,
        privateRoot.absolutePath,
        true,
        false,
      )
      println("TINE_ANDROID_MANAGED_RESUME_RECEIPT $result")
      assertTrue(result, result.startsWith("ok "))
      assertTrue(result, result.contains("second_device_join=ok"))
      assertTrue(result, !result.contains("return_to_direct=ok"))
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
