package dev.daimonos.remote.network

import dev.daimonos.remote.crypto.DeviceIdentity
import dev.daimonos.remote.data.PairedDaemon
import dev.daimonos.remote.protocol.ClientCapability
import dev.daimonos.remote.protocol.ClientMessage
import dev.daimonos.remote.protocol.ProtocolCodec
import dev.daimonos.remote.protocol.RemoteClientFrame
import dev.daimonos.remote.protocol.RemoteServerFrame
import java.util.ArrayDeque
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.encodeToString
import okhttp3.OkHttpClient
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.assertThrows
import org.junit.Test

class RemoteSessionClientTest {
    @Test
    fun pairingWaitsForCommitBeforeReturningTicket() = runBlocking {
        val socket = FakeSocket(
            RemoteServerFrame.Challenge("challenge"),
            RemoteServerFrame.PairingPending(
                dev.daimonos.remote.protocol.PendingPairing(
                    id = "pairing",
                    deviceId = "device",
                    fingerprint = "fingerprint",
                    label = "phone",
                    requestedCapabilities = listOf(ClientCapability.OBSERVE),
                ),
            ),
            RemoteServerFrame.PairingApproved(
                dev.daimonos.remote.protocol.TicketGrant(
                    ticket = "ticket",
                    deviceId = "device",
                    capabilities = listOf(ClientCapability.OBSERVE),
                ),
            ),
            RemoteServerFrame.PairingCommitted("device"),
        )
        val client = client(socket)

        val daemon = client.pair(
            endpoint = "wss://host/v2/ws",
            claim = "claim",
            label = "phone",
            identity = FakeIdentity,
            capabilities = listOf(ClientCapability.OBSERVE),
        )

        assertEquals("ticket", daemon.ticket)
        assertTrue(
            ProtocolCodec.json.decodeFromString<RemoteClientFrame>(socket.sent.single()) is
                RemoteClientFrame.Pair,
        )
        assertTrue(socket.closed)
    }

    @Test
    fun authenticationSignsChallengeAndResumesRetainedSession() = runBlocking {
        val socket = FakeSocket(
            RemoteServerFrame.Challenge("challenge"),
            RemoteServerFrame.Authenticated(
                deviceId = "device",
                capabilities = listOf(ClientCapability.OBSERVE),
            ),
        )
        val client = client(socket)

        client.connect(
            daemon = PairedDaemon(
                endpoint = "wss://host/v2/ws",
                deviceId = "device",
                ticket = "ticket",
                capabilities = setOf(ClientCapability.OBSERVE),
            ),
            identity = FakeIdentity,
            clientLabel = "phone",
            sessionId = "session",
            lastSeenSeq = 42,
        )

        val auth = ProtocolCodec.json.decodeFromString<RemoteClientFrame>(socket.sent[0])
        assertEquals("c2lnbmF0dXJl", (auth as RemoteClientFrame.Authenticate).signature)
        val attach = ProtocolCodec.json.decodeFromString<ClientMessage>(socket.sent[1])
        assertEquals(42, (attach as ClientMessage.Resume).lastSeenSeq)
    }

    @Test
    fun approvedTicketSurvivesLostCommitFrame() = runBlocking {
        val socket = FakeSocket(
            RemoteServerFrame.Challenge("challenge"),
            RemoteServerFrame.PairingApproved(
                dev.daimonos.remote.protocol.TicketGrant(
                    ticket = "provisional",
                    deviceId = "device",
                    capabilities = listOf(ClientCapability.OBSERVE),
                ),
            ),
        )

        val daemon = client(socket).pair(
            endpoint = "wss://host/v2/ws",
            claim = "claim",
            label = "phone",
            identity = FakeIdentity,
            capabilities = listOf(ClientCapability.OBSERVE),
        )

        assertEquals("provisional", daemon.ticket)
    }

    @Test
    fun mismatchedCommitIsNotRecoveredAsTransportLoss() {
        val socket = FakeSocket(
            RemoteServerFrame.Challenge("challenge"),
            RemoteServerFrame.PairingApproved(
                dev.daimonos.remote.protocol.TicketGrant(
                    ticket = "ticket",
                    deviceId = "expected",
                    capabilities = listOf(ClientCapability.OBSERVE),
                ),
            ),
            RemoteServerFrame.PairingCommitted("attacker"),
        )

        assertThrows(IllegalStateException::class.java) {
            runBlocking {
                client(socket).pair(
                    endpoint = "wss://host/v2/ws",
                    claim = "claim",
                    label = "phone",
                    identity = FakeIdentity,
                    capabilities = listOf(ClientCapability.OBSERVE),
                )
            }
        }
    }

    private fun client(socket: FakeSocket) = RemoteSessionClient(
        httpClient = OkHttpClient(),
        connectionFactory = { _, _ -> socket },
    )

    private object FakeIdentity : DeviceIdentity {
        override val publicKey: ByteArray = "public".encodeToByteArray()
        override fun sign(message: ByteArray): ByteArray = "signature".encodeToByteArray()
    }

    private class FakeSocket(vararg frames: RemoteServerFrame) : RemoteSocket {
        private val incoming = ArrayDeque<WebSocketEvent>().apply {
            add(WebSocketEvent.Open)
            frames.forEach {
                add(WebSocketEvent.Text(ProtocolCodec.json.encodeToString(it)))
            }
            add(WebSocketEvent.Closed(1006, "connection lost"))
        }
        val sent = mutableListOf<String>()
        var closed = false
            private set

        override fun send(text: String): Boolean {
            sent += text
            return true
        }

        override suspend fun receive(): WebSocketEvent = incoming.removeFirst()

        override fun close() {
            closed = true
        }
    }
}
