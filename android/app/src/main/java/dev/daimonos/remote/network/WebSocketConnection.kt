package dev.daimonos.remote.network

import java.io.Closeable
import java.net.URI
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.receiveAsFlow
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener

class WebSocketConnection private constructor(
    private val socket: WebSocket,
    private val channel: Channel<WebSocketEvent>,
) : Closeable {
    val events: Flow<WebSocketEvent> = channel.receiveAsFlow()

    fun send(text: String): Boolean = socket.send(text)

    override fun close() {
        socket.close(NORMAL_CLOSURE, "client closing")
    }

    companion object {
        private const val NORMAL_CLOSURE = 1000

        fun open(
            client: OkHttpClient,
            url: String,
        ): WebSocketConnection {
            requireSecureWebSocketUrl(url)
            val channel = Channel<WebSocketEvent>(capacity = 256)
            val listener = object : WebSocketListener() {
                private fun emit(webSocket: WebSocket, event: WebSocketEvent) {
                    if (channel.trySend(event).isFailure) {
                        webSocket.close(1011, "client backpressure")
                    }
                }

                override fun onOpen(webSocket: WebSocket, response: Response) {
                    emit(webSocket, WebSocketEvent.Open)
                }

                override fun onMessage(webSocket: WebSocket, text: String) {
                    emit(webSocket, WebSocketEvent.Text(text))
                }

                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                    webSocket.close(code, reason)
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    emit(webSocket, WebSocketEvent.Closed(code, reason))
                    channel.close()
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    emit(webSocket, WebSocketEvent.Failure(t))
                    channel.close(t)
                }
            }
            val socket = client.newWebSocket(Request.Builder().url(url).build(), listener)
            return WebSocketConnection(socket, channel)
        }

        internal fun requireSecureWebSocketUrl(url: String) {
            val uri = URI(url)
            require(uri.scheme.equals("wss", ignoreCase = true)) {
                "Daimonos remote connections require wss://"
            }
            require(!uri.host.isNullOrBlank()) {
                "Daimonos remote URL requires a host"
            }
        }
    }
}

sealed interface WebSocketEvent {
    data object Open : WebSocketEvent
    data class Text(val value: String) : WebSocketEvent
    data class Closed(val code: Int, val reason: String) : WebSocketEvent
    data class Failure(val error: Throwable) : WebSocketEvent
}
