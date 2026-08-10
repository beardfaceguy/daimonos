package dev.daimonos.remote.protocol

import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json

object ProtocolCodec {
    val json: Json = Json {
        classDiscriminator = "type"
        encodeDefaults = false
        explicitNulls = true
        ignoreUnknownKeys = true
    }

    fun encode(message: ClientMessage): String = json.encodeToString(message)

    fun decodeServer(text: String): ServerMessage = json.decodeFromString(text)

    fun encodeRemote(frame: RemoteClientFrame): String = json.encodeToString(frame)

    fun decodeRemoteServer(text: String): RemoteServerFrame = json.decodeFromString(text)
}
