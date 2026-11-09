package com.wzp.plugin

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

/**
 * Foreground service (type dataSync) that keeps the process alive while a
 * transfer runs and shows an ongoing notification with progress. Tapping the
 * notification opens the app.
 *
 * All companion state is touched only on the main thread (Tauri plugin
 * commands and Service callbacks both run there). start/update/stop are
 * idempotent.
 */
class TransferService : Service() {
  companion object {
    private const val TAG = "WzpTransferService"
    const val CHANNEL_ID = "wzp_transfers"
    const val NOTIFICATION_ID = 0x575A50
    private const val MIN_UPDATE_INTERVAL_MS = 500L

    private val main = Handler(Looper.getMainLooper())
    private var title = "WarzonePhone"
    private var text = ""
    private var current: Long? = null
    private var total: Long? = null

    private var instance: TransferService? = null
    /** startForegroundService() issued, onStartCommand() not yet run. */
    private var starting = false
    private var stopRequested = false
    private var lastPostAt = 0L
    private var refreshScheduled = false

    fun start(context: Context, newTitle: String, newText: String) {
      title = newTitle
      text = newText
      current = null
      total = null
      stopRequested = false
      val svc = instance
      if (svc != null) {
        svc.postNotification()
        return
      }
      if (starting) return
      ensureChannel(context)
      starting = true
      try {
        ContextCompat.startForegroundService(context, Intent(context, TransferService::class.java))
      } catch (e: Exception) {
        starting = false
        // ForegroundServiceStartNotAllowedException (API 31+) when in background.
        throw IllegalStateException(
          "Cannot start the transfer service (the app must be in the foreground): ${e.message}", e
        )
      }
    }

    fun update(newText: String, newCurrent: Long?, newTotal: Long?) {
      text = newText
      current = newCurrent
      total = newTotal
      val svc = instance ?: return
      val wait = lastPostAt + MIN_UPDATE_INTERVAL_MS - SystemClock.elapsedRealtime()
      if (wait <= 0) {
        svc.postNotification()
      } else if (!refreshScheduled) {
        refreshScheduled = true
        main.postDelayed({
          refreshScheduled = false
          instance?.postNotification()
        }, wait)
      }
    }

    fun stop() {
      val svc = instance
      if (svc != null) {
        svc.shutdown()
      } else if (starting) {
        // onStartCommand must still call startForeground() (or the system
        // crashes the app), so let it run and stop right after.
        stopRequested = true
      }
    }

    private fun ensureChannel(context: Context) {
      val nm = context.getSystemService(NotificationManager::class.java) ?: return
      if (nm.getNotificationChannel(CHANNEL_ID) == null) {
        val ch = NotificationChannel(CHANNEL_ID, "File transfers", NotificationManager.IMPORTANCE_LOW)
        ch.description = "Progress of ongoing WarzonePhone file transfers"
        ch.setShowBadge(false)
        nm.createNotificationChannel(ch)
      }
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    instance = this
    starting = false
    ensureChannel(this)
    try {
      val n = buildNotification()
      if (Build.VERSION.SDK_INT >= 29) {
        startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
      } else {
        startForeground(NOTIFICATION_ID, n)
      }
      lastPostAt = SystemClock.elapsedRealtime()
    } catch (e: Exception) {
      Log.e(TAG, "startForeground failed", e)
      shutdown()
      return START_NOT_STICKY
    }
    if (stopRequested) shutdown()
    return START_NOT_STICKY
  }

  /** Android 15+: dataSync services get ~6 h per 24 h; must stop when told. */
  override fun onTimeout(startId: Int, fgsType: Int) {
    Log.w(TAG, "foreground service timeout (type $fgsType), stopping")
    shutdown()
  }

  override fun onDestroy() {
    if (instance === this) instance = null
    super.onDestroy()
  }

  private fun shutdown() {
    stopRequested = false
    if (instance === this) instance = null
    try {
      stopForeground(STOP_FOREGROUND_REMOVE)
    } catch (_: Exception) {}
    stopSelf()
  }

  private fun postNotification() {
    lastPostAt = SystemClock.elapsedRealtime()
    if (Build.VERSION.SDK_INT >= 33 &&
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
    ) {
      return
    }
    getSystemService(NotificationManager::class.java)?.notify(NOTIFICATION_ID, buildNotification())
  }

  private fun buildNotification(): Notification {
    val launch = packageManager.getLaunchIntentForPackage(packageName)?.apply {
      addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED)
    }
    val b = NotificationCompat.Builder(this, CHANNEL_ID)
      .setSmallIcon(R.drawable.wzp_ic_transfer)
      .setContentTitle(title)
      .setContentText(text)
      .setOngoing(true)
      .setOnlyAlertOnce(true)
      .setSilent(true)
      .setCategory(NotificationCompat.CATEGORY_PROGRESS)
      .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
    if (launch != null) {
      b.setContentIntent(
        PendingIntent.getActivity(
          this, 0, launch, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        )
      )
    }
    val t = total
    if (t != null) {
      if (t <= 0L) {
        b.setProgress(0, 0, true)
      } else {
        val c = (current ?: 0L).coerceIn(0L, t)
        b.setProgress(1000, ((c.toDouble() / t.toDouble()) * 1000).toInt(), false)
      }
    }
    return b.build()
  }
}
