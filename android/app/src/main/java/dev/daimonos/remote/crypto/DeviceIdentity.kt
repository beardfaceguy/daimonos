package dev.daimonos.remote.crypto

import java.nio.charset.StandardCharsets
import java.util.Base64

interface DeviceIdentity {
    val publicKey: ByteArray
    fun sign(message: ByteArray): ByteArray

    val encodedPublicKey: String
        get() = publicKey.base64Url()
}

fun authMessage(challenge: String, ticket: String): ByteArray {
    val domain = "daimonos-remote-v2\u0000"
    return (domain + challenge + "\u0000" + ticket)
        .toByteArray(StandardCharsets.UTF_8)
}

fun ByteArray.base64Url(): String =
    Base64.getUrlEncoder().withoutPadding().encodeToString(this)
