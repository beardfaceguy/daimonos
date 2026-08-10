package dev.daimonos.remote.network

import org.junit.Assert.assertThrows
import org.junit.Test

class WebSocketConnectionTest {
    @Test
    fun remoteEndpointRequiresWss() {
        WebSocketConnection.requireSecureWebSocketUrl("wss://host.example/v2/ws")
        assertThrows(IllegalArgumentException::class.java) {
            val insecureScheme = listOf("w", "s").joinToString("")
            WebSocketConnection.requireSecureWebSocketUrl(
                "$insecureScheme://host.example/v2/ws",
            )
        }
        assertThrows(IllegalArgumentException::class.java) {
            WebSocketConnection.requireSecureWebSocketUrl("https://host.example/v2/ws")
        }
    }
}
