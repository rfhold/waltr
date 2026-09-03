package dev.waltr.app

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean

class WaltrForegroundService : Service() {
  private external fun nativeStartServer(dataRoot: String, configRoot: String): Int
  private external fun nativeStopServer()
  private external fun nativeServerStatus(): Int

  private val workerStarted = AtomicBoolean(false)

  override fun onCreate() {
    super.onCreate()
    promote()
    loadNativeLibrary()
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    promote()
    if (nativeServerStatus() != RUNNING && workerStarted.compareAndSet(false, true)) {
      Thread({
        nativeStartServer(filesDir.absolutePath, File(filesDir, "config").absolutePath)
        workerStarted.set(false)
      }, "waltr-server-start").start()
    }
    return START_STICKY
  }

  override fun onDestroy() {
    nativeStopServer()
    super.onDestroy()
  }

  override fun onBind(intent: Intent?): IBinder? = null

  private fun promote() {
    val manager = getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      manager.createNotificationChannel(
        NotificationChannel(
          CHANNEL_ID,
          getString(R.string.server_notification_channel),
          NotificationManager.IMPORTANCE_LOW
        )
      )
    }
    val launchIntent = packageManager.getLaunchIntentForPackage(packageName)
    val pendingIntent = PendingIntent.getActivity(
      this,
      0,
      launchIntent,
      PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
    )
    val notification = NotificationCompat.Builder(this, CHANNEL_ID)
      .setSmallIcon(R.drawable.ic_server_notification)
      .setContentTitle(getString(R.string.server_notification_title))
      .setContentText(getString(R.string.server_notification_text))
      .setContentIntent(pendingIntent)
      .setOngoing(true)
      .setCategory(NotificationCompat.CATEGORY_SERVICE)
      .build()
    startForeground(NOTIFICATION_ID, notification)
  }

  companion object {
    private const val CHANNEL_ID = "waltr_embedded_server"
    private const val NOTIFICATION_ID = 14333
    private const val RUNNING = 2

    private var nativeLibraryLoaded = false

    @Synchronized
    private fun loadNativeLibrary() {
      if (!nativeLibraryLoaded) {
        System.loadLibrary("waltr_app_lib")
        nativeLibraryLoaded = true
      }
    }
  }
}
