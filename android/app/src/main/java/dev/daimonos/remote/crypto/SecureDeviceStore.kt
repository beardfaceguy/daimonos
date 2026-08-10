package dev.daimonos.remote.crypto

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import dev.daimonos.remote.data.CredentialStore
import dev.daimonos.remote.data.PairedDaemon
import dev.daimonos.remote.protocol.ProtocolCodec
import java.security.KeyStore
import java.security.SecureRandom
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.encodeToString
import org.bouncycastle.math.ec.rfc8032.Ed25519

class Ed25519DeviceIdentity internal constructor(
    private val seed: ByteArray,
) : DeviceIdentity {
    init {
        require(seed.size == Ed25519.SECRET_KEY_SIZE)
    }

    override val publicKey: ByteArray = ByteArray(Ed25519.PUBLIC_KEY_SIZE).also {
        Ed25519.generatePublicKey(seed, 0, it, 0)
    }

    override fun sign(message: ByteArray): ByteArray =
        ByteArray(Ed25519.SIGNATURE_SIZE).also { signature ->
            Ed25519.sign(seed, 0, publicKey, 0, message, 0, message.size, signature, 0)
        }
}

class SecureDeviceStore(
    context: Context,
) : CredentialStore {
    private val applicationContext = context.applicationContext
    private val preferences = applicationContext.getSharedPreferences(
        PREFERENCES_NAME,
        Context.MODE_PRIVATE,
    )
    private val mutex = Mutex()

    suspend fun getOrCreateIdentity(): DeviceIdentity = withContext(Dispatchers.IO) {
        mutex.withLock {
            val encrypted = preferences.getString(IDENTITY_KEY, null)
            val seed = if (encrypted == null) {
                ByteArray(Ed25519.SECRET_KEY_SIZE).also(SecureRandom()::nextBytes).also {
                    check(preferences.edit().putString(IDENTITY_KEY, encrypt(it)).commit()) {
                        "unable to persist device identity"
                    }
                }
            } else {
                decrypt(encrypted)
            }
            Ed25519DeviceIdentity(seed)
        }
    }

    override suspend fun load(): PairedDaemon? = withContext(Dispatchers.IO) {
        mutex.withLock {
            preferences.getString(CREDENTIAL_KEY, null)?.let { encrypted ->
                ProtocolCodec.json.decodeFromString<PairedDaemon>(
                    decrypt(encrypted).decodeToString(),
                )
            }
        }
    }

    override suspend fun save(daemon: PairedDaemon) = withContext(Dispatchers.IO) {
        mutex.withLock {
            val encoded = ProtocolCodec.json.encodeToString(daemon).encodeToByteArray()
            check(preferences.edit().putString(CREDENTIAL_KEY, encrypt(encoded)).commit()) {
                "unable to persist paired daemon"
            }
        }
    }

    override suspend fun clear() = withContext(Dispatchers.IO) {
        mutex.withLock {
            check(preferences.edit().remove(CREDENTIAL_KEY).commit()) {
                "unable to clear paired daemon"
            }
        }
    }

    suspend fun resetIdentity() = withContext(Dispatchers.IO) {
        mutex.withLock {
            check(preferences.edit().clear().commit()) {
                "unable to reset device identity"
            }
            keyStore().deleteEntry(MASTER_KEY_ALIAS)
        }
    }

    private fun encrypt(plaintext: ByteArray): String {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, masterKey())
        val ciphertext = cipher.doFinal(plaintext)
        val envelope = byteArrayOf(FORMAT_VERSION, cipher.iv.size.toByte()) + cipher.iv + ciphertext
        return Base64.getUrlEncoder().withoutPadding().encodeToString(envelope)
    }

    private fun decrypt(encoded: String): ByteArray {
        val envelope = Base64.getUrlDecoder().decode(encoded)
        require(envelope.size > 2 && envelope[0] == FORMAT_VERSION) {
            "unsupported secure-store format"
        }
        val ivLength = envelope[1].toInt() and 0xff
        require(ivLength in 12..16 && envelope.size > 2 + ivLength) {
            "invalid secure-store envelope"
        }
        val iv = envelope.copyOfRange(2, 2 + ivLength)
        val ciphertext = envelope.copyOfRange(2 + ivLength, envelope.size)
        val cipher = Cipher.getInstance(TRANSFORMATION)
        // Decryption must reuse the unique provider-generated IV stored with this ciphertext.
        cipher.init(Cipher.DECRYPT_MODE, masterKey(), GCMParameterSpec(128, iv)) // nosemgrep
        return cipher.doFinal(ciphertext)
    }

    private fun masterKey(): SecretKey {
        val keyStore = keyStore()
        (keyStore.getKey(MASTER_KEY_ALIAS, null) as? SecretKey)?.let { return it }
        return KeyGenerator.getInstance(
            KeyProperties.KEY_ALGORITHM_AES,
            ANDROID_KEYSTORE,
        ).run {
            init(
                KeyGenParameterSpec.Builder(
                    MASTER_KEY_ALIAS,
                    KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                )
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setKeySize(256)
                    .build(),
            )
            generateKey()
        }
    }

    private fun keyStore(): KeyStore =
        KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }

    private companion object {
        const val PREFERENCES_NAME = "daimonos_secure"
        const val IDENTITY_KEY = "device_identity"
        const val CREDENTIAL_KEY = "paired_daemon"
        const val MASTER_KEY_ALIAS = "daimonos_remote_master"
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val FORMAT_VERSION: Byte = 1
    }
}
