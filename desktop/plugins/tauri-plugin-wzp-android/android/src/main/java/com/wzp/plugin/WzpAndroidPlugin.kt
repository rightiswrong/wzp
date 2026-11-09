package com.wzp.plugin

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.WebView
import androidx.activity.result.ActivityResult
import app.tauri.PermissionState
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.util.concurrent.Executors

@InvokeArg
class SaveToDownloadsArgs {
  lateinit var path: String
  lateinit var name: String
  var mime: String? = null
}

@InvokeArg
class StartTransferServiceArgs {
  var title: String = "WarzonePhone"
  var text: String = ""
}

@InvokeArg
class UpdateTransferServiceArgs {
  var text: String = ""
  var current: Long? = null
  var total: Long? = null
}

/**
 * Kotlin half of tauri-plugin-wzp-android. Commands are invoked from Rust
 * (see src/mobile.rs) and run on the main thread; anything that touches large
 * files is moved to a background executor and resolves from there.
 */
@TauriPlugin(
  permissions = [
    Permission(strings = [Manifest.permission.POST_NOTIFICATIONS], alias = WzpAndroidPlugin.NOTIFICATIONS),
    Permission(strings = [Manifest.permission.WRITE_EXTERNAL_STORAGE], alias = WzpAndroidPlugin.STORAGE),
  ]
)
class WzpAndroidPlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val TAG = "WzpAndroidPlugin"
    const val NOTIFICATIONS = "notifications"
    const val STORAGE = "storage"
  }

  private val io = Executors.newCachedThreadPool()
  private val main = Handler(Looper.getMainLooper())

  override fun load(webView: WebView) {
    super.load(webView)
    ShareInbox.listener = { files -> trigger("shared", filesResponse(files)) }
    // Cold-start share intents are handled by MainActivity.onCreate; this is
    // a no-op if it already ran for this Intent.
    ShareInbox.handleIntent(activity, activity.intent)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    ShareInbox.handleIntent(activity, intent)
  }

  private fun background(invoke: Invoke, block: () -> Unit) {
    io.execute {
      try {
        block()
      } catch (e: Exception) {
        Log.e(TAG, "${invoke.command} failed", e)
        invoke.reject(e.message ?: e.toString())
      }
    }
  }

  // ---- pickFiles ---------------------------------------------------------

  @Command
  fun pickFiles(invoke: Invoke) {
    val intent = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
      addCategory(Intent.CATEGORY_OPENABLE)
      type = "*/*"
      putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
    }
    startActivityForResult(invoke, intent, "pickFilesResult")
  }

  @ActivityCallback
  fun pickFilesResult(invoke: Invoke, result: ActivityResult) {
    val data = result.data
    if (result.resultCode != Activity.RESULT_OK || data == null) {
      invoke.resolve(filesResponse(emptyList()))
      return
    }
    val uris = LinkedHashSet<Uri>()
    data.clipData?.let { clip ->
      for (i in 0 until clip.itemCount) clip.getItemAt(i).uri?.let { uris.add(it) }
    }
    data.data?.let { uris.add(it) }
    val ctx = activity.applicationContext
    background(invoke) {
      invoke.resolve(filesResponse(FileImporter.import(ctx, uris.toList(), "picked")))
    }
  }

  // ---- share target --------------------------------------------------------

  @Command
  fun takeSharedFiles(invoke: Invoke) {
    invoke.resolve(filesResponse(ShareInbox.take()))
  }

  // ---- saveToDownloads ---------------------------------------------------

  @Command
  fun saveToDownloads(invoke: Invoke) {
    invoke.parseArgs(SaveToDownloadsArgs::class.java)
    if (Build.VERSION.SDK_INT < 29 && getPermissionState(STORAGE) != PermissionState.GRANTED) {
      requestPermissionForAlias(STORAGE, invoke, "storagePermissionResult")
      return
    }
    doSave(invoke)
  }

  @PermissionCallback
  fun storagePermissionResult(invoke: Invoke) {
    doSave(invoke)
  }

  private fun doSave(invoke: Invoke) {
    val ctx = activity.applicationContext
    background(invoke) {
      val args = invoke.parseArgs(SaveToDownloadsArgs::class.java)
      val src = File(args.path)
      val location = if (Build.VERSION.SDK_INT >= 29) {
        DownloadsSaver.saveMediaStore(ctx, src, args.name, args.mime)
      } else {
        val granted = getPermissionState(STORAGE) == PermissionState.GRANTED
        DownloadsSaver.saveLegacy(ctx, src, args.name, args.mime, granted)
      }
      val r = JSObject()
      r.put("location", location)
      invoke.resolve(r)
    }
  }

  // ---- transfer foreground service ---------------------------------------

  @Command
  fun startTransferService(invoke: Invoke) {
    val args = invoke.parseArgs(StartTransferServiceArgs::class.java)
    try {
      TransferService.start(activity.applicationContext, args.title, args.text)
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject(e.message ?: e.toString())
    }
  }

  @Command
  fun updateTransferService(invoke: Invoke) {
    val args = invoke.parseArgs(UpdateTransferServiceArgs::class.java)
    TransferService.update(args.text, args.current, args.total)
    invoke.resolve()
  }

  @Command
  fun stopTransferService(invoke: Invoke) {
    TransferService.stop()
    invoke.resolve()
  }

  // ---- notification permission -------------------------------------------

  @Command
  fun requestNotificationPermission(invoke: Invoke) {
    if (Build.VERSION.SDK_INT < 33 || getPermissionState(NOTIFICATIONS) == PermissionState.GRANTED) {
      resolveGranted(invoke, true)
      return
    }
    requestPermissionForAlias(NOTIFICATIONS, invoke, "notificationPermissionResult")
  }

  @PermissionCallback
  fun notificationPermissionResult(invoke: Invoke) {
    resolveGranted(invoke, getPermissionState(NOTIFICATIONS) == PermissionState.GRANTED)
  }

  private fun resolveGranted(invoke: Invoke, granted: Boolean) {
    val r = JSObject()
    r.put("granted", granted)
    invoke.resolve(r)
  }

  // ---- app data dir ------------------------------------------------------

  @Command
  fun appDataDir(invoke: Invoke) {
    val dir = File(activity.filesDir, "wzp")
    dir.mkdirs()
    val r = JSObject()
    r.put("path", dir.absolutePath)
    invoke.resolve(r)
  }
}
