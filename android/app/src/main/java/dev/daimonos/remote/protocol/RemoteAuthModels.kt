@file:OptIn(kotlinx.serialization.ExperimentalSerializationApi::class)

package dev.daimonos.remote.protocol

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonClassDiscriminator

@Serializable
@JsonClassDiscriminator("type")
sealed interface RemoteClientFrame {
    @Serializable
    @SerialName("pair")
    data class Pair(
        val claim: String,
        @SerialName("device_public_key") val devicePublicKey: String,
        val label: String,
        @SerialName("requested_capabilities")
        val requestedCapabilities: List<ClientCapability>,
    ) : RemoteClientFrame

    @Serializable
    @SerialName("authenticate")
    data class Authenticate(
        val ticket: String,
        @SerialName("device_public_key") val devicePublicKey: String,
        val signature: String,
    ) : RemoteClientFrame
}

@Serializable
@JsonClassDiscriminator("type")
sealed interface RemoteServerFrame {
    @Serializable
    @SerialName("challenge")
    data class Challenge(val challenge: String) : RemoteServerFrame

    @Serializable
    @SerialName("pairing_pending")
    data class PairingPending(val pairing: PendingPairing) : RemoteServerFrame

    @Serializable
    @SerialName("pairing_approved")
    data class PairingApproved(val grant: TicketGrant) : RemoteServerFrame

    @Serializable
    @SerialName("pairing_committed")
    data class PairingCommitted(
        @SerialName("device_id") val deviceId: String,
    ) : RemoteServerFrame

    @Serializable
    @SerialName("authenticated")
    data class Authenticated(
        @SerialName("device_id") val deviceId: String,
        val capabilities: List<ClientCapability>,
    ) : RemoteServerFrame

    @Serializable
    @SerialName("error")
    data class Error(
        val code: String,
        val message: String,
    ) : RemoteServerFrame
}

@Serializable
data class PendingPairing(
    val id: String,
    @SerialName("device_id") val deviceId: String,
    val fingerprint: String,
    val label: String,
    @SerialName("requested_capabilities")
    val requestedCapabilities: List<ClientCapability>,
)

@Serializable
data class TicketGrant(
    val ticket: String,
    @SerialName("device_id") val deviceId: String,
    val capabilities: List<ClientCapability>,
)
