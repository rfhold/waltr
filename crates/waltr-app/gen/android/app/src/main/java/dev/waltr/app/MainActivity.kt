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
  private companion object {
    val audioProcessIdentity = java.util.UUID.randomUUID().toString()
  }
  private external fun nativeNotificationPermissionRequired(required: Boolean)
  private external fun nativeClientSuspend()
  private external fun nativeClientResume()
  private external fun nativeAuthorizationCallback(uri: String)
  private external fun nativeInitializeCredentialStore(store: SecureTokenStore)
  private var localSelected = false
  private external fun nativeRecordAudioPermissionResult(ticket: Long, granted: Boolean)
  private external fun nativeRecordAudioPermissionCurrent(ticket: Long): Boolean
  private var audioTicket: Long? = null
  private var audioResumed = false
  private val recordAudioPermission = registerForActivityResult(
    ActivityResultContracts.RequestPermission()
  ) { granted ->
    val ticket = audioTicket
    audioTicket = null
    if (ticket != null) nativeRecordAudioPermissionResult(ticket, granted)
  }

  fun requestRecordAudioPermission(ticket: Long) {
    runOnUiThread {
      if (!audioResumed || !nativeRecordAudioPermissionCurrent(ticket)) {
        nativeRecordAudioPermissionResult(ticket, false)
        return@runOnUiThread
      }
      // A Boolean callback belongs only to the original outstanding dialog.
      if (audioTicket != null) {
        nativeRecordAudioPermissionResult(ticket, false)
        return@runOnUiThread
      }
      if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) ==
        PackageManager.PERMISSION_GRANTED
      ) {
        nativeRecordAudioPermissionResult(ticket, true)
        return@runOnUiThread
      }
      audioTicket = ticket
      RecordAudioPermissionLaunch.run({
        recordAudioPermission.launch(Manifest.permission.RECORD_AUDIO)
      }, {
        if (audioTicket == ticket) {
          audioTicket = null
          nativeRecordAudioPermissionResult(ticket, false)
        }
      })
    }
  }

  override fun onSaveInstanceState(outState: Bundle) {
    audioTicket?.let {
      outState.putLong("waltr.audioPermissionTicket", it)
      outState.putString("waltr.audioPermissionProcess", audioProcessIdentity)
    }
    super.onSaveInstanceState(outState)
  }

  private var permissionRequested = false
  private val notificationPermission = registerForActivityResult(
    ActivityResultContracts.RequestPermission()
  ) { granted ->
    nativeNotificationPermissionRequired(!granted)
    if (granted && localSelected) startWaltrService()
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    if (savedInstanceState?.containsKey("waltr.audioPermissionTicket") == true) {
      // A restored dialog from a dead process must never match a new Rust ticket.
      audioTicket = if (savedInstanceState.getString("waltr.audioPermissionProcess") == audioProcessIdentity) {
        savedInstanceState.getLong("waltr.audioPermissionTicket")
      } else -1L
    }
    super.onCreate(savedInstanceState)
    nativeInitializeCredentialStore(SecureTokenStore.initialize(this))
  }

  override fun onPause() {
    audioResumed = false
    nativeClientSuspend()
    super.onPause()
  }

  override fun onResume() {
    super.onResume()
    audioResumed = true
    if (localSelected) ensureNotificationPermissionAndServer()
    nativeClientResume()
  }

  fun requestLocalServer() {
    runOnUiThread {
      localSelected = true
      ensureNotificationPermissionAndServer()
    }
  }

  fun selectRemoteServer() {
    runOnUiThread { localSelected = false }
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    if (intent.action == Intent.ACTION_VIEW) {
      val uri = intent.dataString
      if (uri != null && uri.length <= 8192) nativeAuthorizationCallback(uri)
    }
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
