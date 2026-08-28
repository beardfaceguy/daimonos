package dev.daimonos.remote.protocol

import dev.daimonos.remote.crypto.authMessage
import java.security.KeyFactory
import java.security.Signature
import java.security.spec.X509EncodedKeySpec
import java.util.Base64
import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.decodeFromJsonElement
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.assertThrows
import org.junit.Test

class ProtocolFixtureTest {
    @Test
    fun attachFixtureRoundTripsWithoutWireDrift() {
        val source = fixture("attach_request.json")
        val decoded = ProtocolCodec.json.decodeFromString<ClientMessage>(source)

        assertTrue(decoded is ClientMessage.Attach)
        assertEquals(
            ProtocolCodec.json.parseToJsonElement(source),
            ProtocolCodec.json.parseToJsonElement(ProtocolCodec.encode(decoded)),
        )
    }

    @Test
    fun snapshotFixtureDecodesCanonicalState() {
        val decoded = ProtocolCodec.decodeServer(fixture("snapshot.json"))
        val snapshot = decoded as ServerMessage.Snapshot

        assertEquals("session-fixture", snapshot.state.sessionId)
        assertEquals(TurnStatus.WAITING_FOR_APPROVAL, snapshot.state.turnStatus)
        assertEquals(2, snapshot.state.transcript.size)
        assertEquals(ToolCallStatus.PENDING, snapshot.state.toolCalls.single().status)
        assertEquals(2500, snapshot.state.contextUsage?.utilizationBasisPoints)
        assertFalse(snapshot.state.historyTruncated)
    }

    @Test
    fun eventAndCommandFixturesDecodeEveryEnvelope() {
        val events = ProtocolCodec.json
            .parseToJsonElement(fixture("event_stream.json"))
            .jsonArray
            .map { ProtocolCodec.json.decodeFromJsonElement<ServerMessage>(it) }
        val commands = ProtocolCodec.json
            .parseToJsonElement(fixture("client_commands.json"))
            .jsonArray
            .map { ProtocolCodec.json.decodeFromJsonElement<ClientMessage>(it) }

        assertEquals(7, events.size)
        assertEquals(8, commands.size)
        assertTrue(events.last() is ServerMessage.Event)
        assertTrue((events.last() as ServerMessage.Event).event is SessionEvent.ConversationCleared)
        assertTrue(commands.first() is ClientMessage.Resume)
        assertTrue(commands.last() is ClientMessage.Detach)
    }

    @Test
    fun runtimeValueRejectsShapesTheRustProtocolCannotAccept() {
        val invalid =
            """{"type":"set_config","config_id":"model","value":["not","allowed"]}"""
        assertThrows(SerializationException::class.java) {
            ProtocolCodec.json.decodeFromString<ClientMessage>(invalid)
        }
    }

    @Test
    fun userMessageCarriesOptionalPromptCorrelationId() {
        val message = ProtocolCodec.decodeServer(
            """{"type":"event","seq":1,"event":{"type":"user_message","text":"hi","request_id":"prompt-1"}}""",
        ) as ServerMessage.Event
        val event = message.event as SessionEvent.UserMessage

        assertEquals("prompt-1", event.requestId)
    }

    @Test
    fun revocationCodeIsTypedButLegacyReasonStillDecodes() {
        val typed = ProtocolCodec.decodeServer(
            """{"type":"revoked","code":"event_queue_lagged","reason":"lagged"}""",
        ) as ServerMessage.Revoked
        assertEquals(RevocationCode.EVENT_QUEUE_LAGGED, typed.code)

        val legacy = ProtocolCodec.decodeServer(
            """{"type":"revoked","reason":"legacy"}""",
        ) as ServerMessage.Revoked
        assertEquals(null, legacy.code)
    }

    @Test
    fun attachDeniedCodeIsTypedButLegacyReasonStillDecodes() {
        val typed = ProtocolCodec.decodeServer(
            """{"type":"attach_denied","code":"client_limit_reached","reason":"full"}""",
        ) as ServerMessage.AttachDenied
        assertEquals(AttachDeniedCode.CLIENT_LIMIT_REACHED, typed.code)

        val legacy = ProtocolCodec.decodeServer(
            """{"type":"attach_denied","reason":"legacy"}""",
        ) as ServerMessage.AttachDenied
        assertEquals(null, legacy.code)
    }

    @Test
    fun sessionListDecodesRichLocalAndMinimalRemoteRows() {
        val rich = ProtocolCodec.decodeServer(
            """
            {"type":"session_list","request_id":"list-1",
             "workspace":{"id":"ws_1","label":"workspace"},
             "sessions":[{"session_id":"session-1","active":true,"attached_clients":1,
             "model":"model","updated_at_unix_ms":42,"preview":"hello",
             "message_count":2,"turn_status":"idle"}],"next_cursor":"v1_cursor",
             "incomplete":true}
            """.trimIndent(),
        ) as ServerMessage.SessionList
        assertEquals("ws_1", rich.workspace?.id)
        assertEquals("hello", rich.sessions.single().preview)
        assertEquals(TurnStatus.IDLE, rich.sessions.single().turnStatus)
        assertTrue(rich.incomplete)

        val minimal = ProtocolCodec.decodeServer(
            """{"type":"session_list","request_id":"list-2","sessions":[{"session_id":"session-1","active":true,"attached_clients":1}]}""",
        ) as ServerMessage.SessionList
        assertEquals(null, minimal.workspace)
        assertEquals(null, minimal.sessions.single().preview)
        assertFalse(minimal.incomplete)
    }

    @Test
    fun remoteAuthFixtureIncludesValidRustCompatibleEd25519Vector() {
        val fixture = ProtocolCodec.json
            .parseToJsonElement(fixture("remote_auth.json"))
            .jsonObject
        listOf(
            "server_challenge",
            "pairing_pending",
            "pairing_approved",
            "pairing_committed",
            "authenticated",
        ).forEach { field ->
            ProtocolCodec.json.decodeFromJsonElement<RemoteServerFrame>(fixture.required(field))
        }
        val challenge = ProtocolCodec.json.decodeFromJsonElement<RemoteServerFrame>(
            fixture.required("server_challenge"),
        ) as RemoteServerFrame.Challenge
        val pair = ProtocolCodec.json.decodeFromJsonElement<RemoteClientFrame>(
            fixture.required("pair_request"),
        ) as RemoteClientFrame.Pair
        val approved = ProtocolCodec.json.decodeFromJsonElement<RemoteServerFrame>(
            fixture.required("pairing_approved"),
        ) as RemoteServerFrame.PairingApproved
        val committed = ProtocolCodec.json.decodeFromJsonElement<RemoteServerFrame>(
            fixture.required("pairing_committed"),
        ) as RemoteServerFrame.PairingCommitted
        val authenticate = ProtocolCodec.json.decodeFromJsonElement<RemoteClientFrame>(
            fixture.required("authenticate_request"),
        ) as RemoteClientFrame.Authenticate
        assertEquals(pair.devicePublicKey, authenticate.devicePublicKey)
        assertEquals(approved.grant.ticket, authenticate.ticket)
        assertEquals(approved.grant.deviceId, committed.deviceId)

        val rawPublicKey = Base64.getUrlDecoder().decode(authenticate.devicePublicKey)
        val subjectPublicKeyInfo = ED25519_X509_PREFIX + rawPublicKey
        val publicKey = KeyFactory.getInstance("Ed25519")
            .generatePublic(X509EncodedKeySpec(subjectPublicKeyInfo))
        val verifier = Signature.getInstance("Ed25519")
        verifier.initVerify(publicKey)
        verifier.update(authMessage(challenge.challenge, authenticate.ticket))

        assertTrue(verifier.verify(Base64.getUrlDecoder().decode(authenticate.signature)))
    }

    private fun fixture(name: String): String =
        checkNotNull(javaClass.classLoader?.getResource(name)) {
            "missing fixture $name"
        }.readText()

    private fun JsonObject.required(name: String) =
        checkNotNull(this[name]) { "missing fixture field $name" }

    private companion object {
        val ED25519_X509_PREFIX = byteArrayOf(
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03,
            0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        )
    }
}
