package dev.waltr.app

import android.content.Intent
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  private external fun nativeReceiveIntent(intent: Intent)
  private external fun nativeSuspend()
  private external fun nativeResume()
  private var wryWebView: WebView? = null

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    wryWebView = webView
    installWebViewInsets()
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    nativeReceiveIntent(intent)
  }

  private fun installWebViewInsets() {
    val webView = wryWebView ?: return
    val initialPaddingLeft = webView.paddingLeft
    val initialPaddingTop = webView.paddingTop
    val initialPaddingRight = webView.paddingRight
    val initialPaddingBottom = webView.paddingBottom

    ViewCompat.setOnApplyWindowInsetsListener(webView) { view, windowInsets ->
      val safeInsets = windowInsets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
      )
      view.setPadding(
        initialPaddingLeft + safeInsets.left,
        initialPaddingTop + safeInsets.top,
        initialPaddingRight + safeInsets.right,
        initialPaddingBottom + safeInsets.bottom,
      )
      windowInsets
    }
    ViewCompat.requestApplyInsets(webView)
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
