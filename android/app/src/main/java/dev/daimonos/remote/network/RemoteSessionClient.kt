package dev.daimonos.remote.network

import dev.daimonos.remote.crypto.DeviceIdentity
import dev.daimonos.remote.crypto.authMessage
import dev.daimonos.remote.crypto.base64Url
import dev.daimonos.remote.data.PairedDaemon
import dev.daimonos.remote.protocol.ApprovalDecision
import dev.daimonos.remote.protocol.ClientCapability
import dev.daimonos.remote.protocol.ClientInfo
import dev.daimonos.remote.protocol.ClientKind
import dev.daimonos.remote.protocol.ClientMessage
import dev.daimonos.remote.protocol.ProtocolCodec
import dev.daimonos.remote.protocol.RemoteClientFrame
import dev.daimonos.remote.protocol.RemoteServerFrame
import dev.daimonos.remote.protocol.ServerMessage
import java.io.Closeable
import java.util.UUID
import kotlinx.coroutines.withTimeout
import okhttp3.OkHttpClient

class RemoteSessionClient(
    private val httpClient: OkHttpClient,
    private val connectionFactory: (OkHttpClient, String) -> RemoteSocket =
        WebSocketConnection.Companion::open,
) {
    suspend fun pair(
        endpoint: String,
        claim: String,
        label: String,
        identity: DeviceIdentity,
        capabilities: List<ClientCapability>,
        onPending: (RemoteServerFrame.PairingPending) -> Unit = {},
    ): PairedDaemon = withTimeout(PAIRING_TIMEOUT_MS) {
        val connection = connectionFactory(httpClient, endpoint)
        var approved: RemoteServerFrame.PairingApproved? = null
        try {
            receiveChallenge(connection)
            check(
                connection.send(
                    ProtocolCodec.encodeRemote(
                        RemoteClientFrame.Pair(
                            claim = claim,
                            devicePublicKey = identity.encodedPublicKey,
                            label = label,
                            requestedCapabilities = capabilities,
                        ),
                    ),
                ),
            ) { "pairing request exceeded the WebSocket send queue" }
            while (true) {
                when (val frame = ProtocolCodec.decodeRemoteServer(connection.receiveText())) {
                    is RemoteServerFrame.PairingPending -> onPending(frame)
                    is RemoteServerFrame.PairingApproved -> approved = frame
                    is RemoteServerFrame.PairingCommitted -> {
                        val grant = checkNotNull(approved) {
                            "pairing committed before ticket approval"
                        }.grant
                        check(grant.deviceId == frame.deviceId) {
                            "pairing device identity changed before commit"
                        }
                        return@withTimeout PairedDaemon(
                            endpoint = endpoint,
                            deviceId = grant.deviceId,
                            ticket = grant.ticket,
                            capabilities = grant.capabilities.toSet(),
                        )
                    }
                    is RemoteServerFrame.Error -> error("${frame.code}: ${frame.message}")
                    else -> error("unexpected pairing frame: $frame")
                }
            }
            error("unreachable")
        } catch (error: RemoteTransportException) {
            val provisional = approved?.grant
            if (provisional != null) {
                return@withTimeout PairedDaemon(
                    endpoint = endpoint,
                    deviceId = provisional.deviceId,
                    ticket = provisional.ticket,
                    capabilities = provisional.capabilities.toSet(),
                )
            }
            throw error
        } finally {
            connection.close()
        }
    }

    suspend fun connect(
        daemon: PairedDaemon,
        identity: DeviceIdentity,
        clientLabel: String,
        sessionId: String?,
        lastSeenSeq: Long?,
    ): SessionConnection = withTimeout(AUTH_TIMEOUT_MS) {
        val connection = connectionFactory(httpClient, daemon.endpoint)
        try {
            val challenge = receiveChallenge(connection)
            check(
                connection.send(
                    ProtocolCodec.encodeRemote(
                        RemoteClientFrame.Authenticate(
                            ticket = daemon.ticket,
                            devicePublicKey = identity.encodedPublicKey,
                            signature = identity
                                .sign(authMessage(challenge, daemon.ticket))
                                .base64Url(),
                        ),
                    ),
                ),
            ) { "authentication exceeded the WebSocket send queue" }
            val authenticated = ProtocolCodec.decodeRemoteServer(connection.receiveText())
            if (authenticated is RemoteServerFrame.Error) {
                throw RemoteAuthenticationException(
                    "${authenticated.code}: ${authenticated.message}",
                )
            }
            check(authenticated is RemoteServerFrame.Authenticated)
            check(authenticated.deviceId == daemon.deviceId) {
                "authenticated device does not match stored pairing"
            }
            val client = ClientInfo(
                id = daemon.deviceId,
                kind = ClientKind.ANDROID,
                label = clientLabel,
            )
            val requested = daemon.capabilities.toList()
            val attach = if (sessionId != null && lastSeenSeq != null) {
                ClientMessage.Resume(
                    sessionId = sessionId,
                    lastSeenSeq = lastSeenSeq,
                    ticket = null,
                    client = client,
                    requestedCapabilities = requested,
                )
            } else {
                ClientMessage.Attach(
                    sessionId = sessionId,
                    ticket = null,
                    client = client,
                    requestedCapabilities = requested,
                )
            }
            check(connection.send(ProtocolCodec.encode(attach))) {
                "attach exceeded the WebSocket send queue"
            }
            SessionConnection(connection)
        } catch (error: Throwable) {
            connection.close()
            throw error
        }
    }

    private suspend fun receiveChallenge(connection: RemoteSocket): String {
        val frame = ProtocolCodec.decodeRemoteServer(connection.receiveText())
        check(frame is RemoteServerFrame.Challenge) {
            "server did not begin with an authentication challenge"
        }
        return frame.challenge
    }

    private suspend fun RemoteSocket.receiveText(): String {
        while (true) {
            when (val event = receive()) {
                WebSocketEvent.Open -> Unit
                is WebSocketEvent.Text -> return event.value
                is WebSocketEvent.Closed -> throw RemoteTransportException(
                    "WebSocket closed (${event.code}): ${event.reason}",
                )
                is WebSocketEvent.Failure -> throw RemoteTransportException(
                    "WebSocket failed",
                    event.error,
                )
            }
        }
    }

    private companion object {
        const val AUTH_TIMEOUT_MS = 15_000L
        const val PAIRING_TIMEOUT_MS = 360_000L
    }
}

class SessionConnection internal constructor(
    private val connection: RemoteSocket,
) : Closeable {
    suspend fun receive(): ServerMessage {
        while (true) {
            when (val event = connection.receive()) {
                WebSocketEvent.Open -> Unit
                is WebSocketEvent.Text -> return ProtocolCodec.decodeServer(event.value)
                is WebSocketEvent.Closed -> throw RemoteTransportException(
                    "WebSocket closed (${event.code}): ${event.reason}",
                )
                is WebSocketEvent.Failure -> throw RemoteTransportException(
                    "WebSocket failed",
                    event.error,
                )
            }
        }
    }

    fun prompt(text: String): String? {
        val requestId = UUID.randomUUID().toString()
        return requestId.takeIf {
            send(ClientMessage.Prompt(requestId, text))
        }
    }

    fun interrupt(): Boolean =
        send(ClientMessage.Interrupt(UUID.randomUUID().toString()))

    fun answerApproval(approvalId: String, decision: ApprovalDecision): Boolean =
        send(ClientMessage.ApprovalResponse(approvalId, decision))

    fun sync(lastSeenSeq: Long): Boolean =
        send(ClientMessage.SyncRequest(lastSeenSeq))

    fun listSessions(cursor: String? = null): Boolean =
        send(ClientMessage.ListSessions(UUID.randomUUID().toString(), cursor))

    fun stopSession(): Boolean =
        send(ClientMessage.StopSession(UUID.randomUUID().toString()))

    fun detach(): Boolean = send(ClientMessage.Detach)

    private fun send(message: ClientMessage): Boolean =
        connection.send(ProtocolCodec.encode(message))

    override fun close() {
        connection.close()
    }
}

class RemoteAuthenticationException(message: String) : IllegalStateException(message)

open class RemoteTransportException(
    message: String,
    cause: Throwable? = null,
) : IllegalStateException(message, cause)
