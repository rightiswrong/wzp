package com.wzp.plugin

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.Parcelable
import android.util.Log
import android.widget.Toast
import java.util.concurrent.Executors

/**
 * Receives "Share to WarzonePhone" intents (ACTION_SEND / ACTION_SEND_MULTIPLE),
 * copies the shared content into the app cache and queues it until the Rust
 * side drains it with takeSharedFiles. Process-wide (survives activity
 * re-creation).
 *
 * Entry points: MainActivity.onCreate (cold start / activity re-creation) and
 * WzpAndroidPlugin.onNewIntent (app already running, singleTask).
 */
object ShareInbox {
  private const val TAG = "WzpShareInbox"
  private const val HANDLED_EXTRA = "com.wzp.plugin.SHARE_HANDLED"

  private val queue = ArrayList<ImportedFile>()
  private val executor = Executors.newSingleThreadExecutor()
  private val main = Handler(Looper.getMainLooper())

  /** Called (on the main thread) with each newly imported batch. */
  @Volatile
  internal var listener: ((List<ImportedFile>) -> Unit)? = null

  /** Safe to call more than once with the same Intent; non-share intents are ignored. */
  @JvmStatic
  fun handleIntent(context: Context, intent: Intent?) {
    if (intent == null) return
    val action = intent.action
    if (action != Intent.ACTION_SEND && action != Intent.ACTION_SEND_MULTIPLE) return
    if (intent.getBooleanExtra(HANDLED_EXTRA, false)) return
    // Re-launch from "recents" re-delivers the original share intent.
    if (intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY != 0) return
    intent.putExtra(HANDLED_EXTRA, true)

    val uris = extractUris(intent)
    val text = if (uris.isEmpty()) intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString() else null
    val subject = intent.getStringExtra(Intent.EXTRA_SUBJECT)
    if (uris.isEmpty() && text.isNullOrEmpty()) {
      Log.w(TAG, "share intent without content")
      return
    }
    val app = context.applicationContext
    executor.execute {
      try {
        val files = if (uris.isNotEmpty()) {
          FileImporter.import(app, uris, "shared")
        } else {
          listOf(FileImporter.importText(app, text!!, subject, "shared"))
        }
        synchronized(queue) { queue.addAll(files) }
        Log.i(TAG, "queued ${files.size} shared file(s)")
        main.post { listener?.invoke(files) }
      } catch (e: Exception) {
        Log.e(TAG, "failed to import shared content", e)
        main.post {
          Toast.makeText(app, "WarzonePhone: could not import shared file: ${e.message}", Toast.LENGTH_LONG).show()
        }
      }
    }
  }

  internal fun take(): List<ImportedFile> = synchronized(queue) {
    val out = ArrayList(queue)
    queue.clear()
    out
  }

  @Suppress("DEPRECATION")
  private fun extractUris(intent: Intent): List<Uri> {
    val uris = LinkedHashSet<Uri>()
    if (intent.action == Intent.ACTION_SEND_MULTIPLE) {
      val list: List<Parcelable>? = if (Build.VERSION.SDK_INT >= 33) {
        intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
      } else {
        intent.getParcelableArrayListExtra<Parcelable>(Intent.EXTRA_STREAM)
      }
      list?.forEach { if (it is Uri) uris.add(it) }
    } else {
      val one: Parcelable? = if (Build.VERSION.SDK_INT >= 33) {
        intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
      } else {
        intent.getParcelableExtra<Parcelable>(Intent.EXTRA_STREAM)
      }
      if (one is Uri) uris.add(one)
    }
    if (uris.isEmpty()) {
      val clip = intent.clipData
      if (clip != null) {
        for (i in 0 until clip.itemCount) clip.getItemAt(i).uri?.let { uris.add(it) }
      }
    }
    return uris.toList()
  }
}
