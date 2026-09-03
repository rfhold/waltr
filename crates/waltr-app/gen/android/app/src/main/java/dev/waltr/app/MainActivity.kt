package dev.waltr.app

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {
  private external fun nativeNotificationPermissionRequired(required: Boolean)
  private external fun nativeClientSuspend()
  private external fun nativeClientResume()

  private var permissionRequested = false
  private val notificationPermission = registerForActivityResult(
    ActivityResultContracts.RequestPermission()
  ) { granted ->
    nativeNotificationPermissionRequired(!granted)
    if (granted) startWaltrService()
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onPause() {
    nativeClientSuspend()
    super.onPause()
  }

  override fun onResume() {
    super.onResume()
    ensureNotificationPermissionAndServer()
    nativeClientResume()
  }

  private fun ensureNotificationPermissionAndServer() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) ==
        PackageManager.PERMISSION_GRANTED
    ) {
      nativeNotificationPermissionRequired(false)
      startWaltrService()
      return
    }

    nativeNotificationPermissionRequired(true)
    if (!permissionRequested) {
      permissionRequested = true
      notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
    }
  }

  private fun startWaltrService() {
    ContextCompat.startForegroundService(this, Intent(this, WaltrForegroundService::class.java))
  }
}
