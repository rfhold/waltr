package dev.waltr.app

import android.content.Context
import android.os.Looper
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.AtomicFile
import androidx.annotation.Keep
import java.io.File
import java.security.KeyStore
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Native-only persistence. No WebView interface, plaintext fallback, or logging. */
@Keep
class SecureTokenStore private constructor(private val context: Context) {
  companion object {
    private const val MAX_TOKEN_BYTES = 32 * 1024
    private const val IV_BYTES = 12
    private const val TAG_BYTES = 16
    private const val HEADER_BYTES = 1 + IV_BYTES
    private const val MAX_RECORD_BYTES = HEADER_BYTES + MAX_TOKEN_BYTES + TAG_BYTES
    private val namespace = Regex("[0-9a-f]{64}")
    @Volatile private var instance: SecureTokenStore? = null

    /** Call after native library load; pass this object to the Rust initialization hook. */
    @JvmStatic
    @Synchronized
    fun initialize(context: Context): SecureTokenStore {
      return instance ?: SecureTokenStore(context.applicationContext).also { instance = it }
    }
  }

  private class MalformedRecord : Exception()

  /** Rust invokes only on a blocking worker. Replies: 0=success/missing, 1+token,
   * 2=unavailable, 3=malformed. No throwable crosses JNI with secret error text. */
  @Synchronized
  fun execute(operation: Int, server: String, account: String, token: ByteArray): ByteArray {
    try {
      if (Looper.myLooper() == Looper.getMainLooper()) return byteArrayOf(2)
      if (!namespace.matches(server) || (operation != 3 && !namespace.matches(account))) {
        return byteArrayOf(3)
      }
      val root = File(context.noBackupFilesDir, "native-refresh-v1")
      if (!root.isDirectory && !root.mkdirs()) return byteArrayOf(2)
      val file = AtomicFile(File(root, "${server}_${account}"))
      return when (operation) {
        0 -> load(file, server, account)
        1 -> {
          if (token.isEmpty() || token.size > MAX_TOKEN_BYTES) return byteArrayOf(3)
          replace(file, server, account, token)
          byteArrayOf(0)
        }
        2 -> {
          delete(file)
          byteArrayOf(0)
        }
        3 -> {
          deleteServer(root, server)
          byteArrayOf(0)
        }
        else -> byteArrayOf(3)
      }
    } catch (_: MalformedRecord) {
      return byteArrayOf(3)
    } catch (_: AEADBadTagException) {
      return byteArrayOf(3)
    } catch (_: Exception) {
      return byteArrayOf(2)
    } finally {
      token.fill(0)
    }
  }

  private fun keyStore(): KeyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
  private fun alias(server: String): String = "dev.waltr.app.refresh.v1.$server"

  private fun existingKey(server: String): SecretKey? {
    val key = keyStore().getKey(alias(server), null) ?: return null
    return key as? SecretKey ?: throw MalformedRecord()
  }

  private fun encryptionKey(server: String): SecretKey {
    existingKey(server)?.let { return it }
    val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
    generator.init(
      KeyGenParameterSpec.Builder(
        alias(server), KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
      ).setKeySize(256)
        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
        .setRandomizedEncryptionRequired(true)
        .build()
    )
    return generator.generateKey()
  }

  private fun aad(server: String, account: String): ByteArray =
    "dev.waltr.app.refresh.v1:$server:$account".toByteArray(Charsets.US_ASCII)

  private fun load(file: AtomicFile, server: String, account: String): ByteArray {
    // openRead recovers AtomicFile's previous complete value after a crash.
    if (!file.baseFile.exists() && !File(file.baseFile.path + ".bak").exists()) {
      return byteArrayOf(0)
    }
    val record = readRecord(file)
    if (record.size !in (HEADER_BYTES + TAG_BYTES + 1)..MAX_RECORD_BYTES) throw MalformedRecord()
    if (record[0] != 1.toByte()) throw MalformedRecord()
    val key = existingKey(server) ?: throw MalformedRecord()
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, record, 1, IV_BYTES))
    cipher.updateAAD(aad(server, account))
    val plaintext = cipher.doFinal(record, HEADER_BYTES, record.size - HEADER_BYTES)
    try {
      if (plaintext.isEmpty() || plaintext.size > MAX_TOKEN_BYTES) throw MalformedRecord()
      return ByteArray(plaintext.size + 1).also {
        it[0] = 1
        plaintext.copyInto(it, 1)
      }
    } finally {
      plaintext.fill(0)
    }
  }

  private fun replace(file: AtomicFile, server: String, account: String, token: ByteArray) {
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    // AndroidKeyStore supplies a fresh randomized IV; callers cannot choose it.
    cipher.init(Cipher.ENCRYPT_MODE, encryptionKey(server))
    val iv = cipher.iv
    if (iv.size != IV_BYTES) throw MalformedRecord()
    cipher.updateAAD(aad(server, account))
    val encrypted = cipher.doFinal(token)
    val record = byteArrayOf(1) + iv + encrypted
    val stream = file.startWrite()
    try {
      stream.write(record)
      stream.fd.sync()
      file.finishWrite(stream)
    } catch (error: Exception) {
      file.failWrite(stream)
      throw error
    }
    // AtomicFile may only log a failed rename; do not acknowledge a lost rotation.
    if (!readRecord(file).contentEquals(record)) throw java.io.IOException()
  }

  private fun readRecord(file: AtomicFile): ByteArray = file.openRead().use { input ->
    val buffer = ByteArray(MAX_RECORD_BYTES + 1)
    var count = 0
    while (count < buffer.size) {
      val read = input.read(buffer, count, buffer.size - count)
      if (read < 0) break
      count += read
    }
    buffer.copyOf(count)
  }

  private fun delete(file: AtomicFile) {
    file.delete()
    if (file.baseFile.exists() || File(file.baseFile.path + ".bak").exists() ||
      File(file.baseFile.path + ".new").exists()) {
      throw java.io.IOException()
    }
  }

  private fun deleteServer(root: File, server: String) {
    // Delete the non-exportable key first, also erasing any orphaned ciphertext.
    keyStore().deleteEntry(alias(server))
    val files = root.listFiles() ?: throw java.io.IOException()
    for (file in files) {
      if (file.name.startsWith("${server}_")) {
        if (!file.delete() && file.exists()) throw java.io.IOException()
      }
    }
    if (keyStore().containsAlias(alias(server))) throw java.io.IOException()
  }
}
