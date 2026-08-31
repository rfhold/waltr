package dev.waltr.app

import android.content.Intent
import android.os.Bundle
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  private external fun nativeReceiveIntent(intent: Intent)
  private external fun nativeSuspend()
  private external fun nativeResume()

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    nativeReceiveIntent(intent)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    nativeReceiveIntent(intent)
  }

  override fun onPause() {
    nativeSuspend()
    super.onPause()
  }

  override fun onResume() {
    super.onResume()
    nativeResume()
  }
}
