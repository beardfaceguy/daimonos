package dev.daimonos.remote.data

import dev.daimonos.remote.protocol.ClientCapability

data class PairedDaemon(
    val endpoint: String,
    val deviceId: String,
    val ticket: String,
    val capabilities: Set<ClientCapability>,
)

interface CredentialStore {
    suspend fun load(): PairedDaemon?
    suspend fun save(daemon: PairedDaemon)
    suspend fun clear()
}
