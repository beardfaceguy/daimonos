package dev.daimonos.remote.ui

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import dev.daimonos.remote.crypto.SecureDeviceStore
import dev.daimonos.remote.crypto.DeviceIdentity
import dev.daimonos.remote.data.PairedDaemon
import dev.daimonos.remote.network.RemoteSessionClient
import dev.daimonos.remote.network.RemoteAuthenticationException
import dev.daimonos.remote.network.SessionConnection
import dev.daimonos.remote.protocol.ApprovalDecision
import dev.daimonos.remote.protocol.ClientCapability
import dev.daimonos.remote.protocol.RevocationCode
import dev.daimonos.remote.protocol.ServerMessage
import dev.daimonos.remote.session.ApplyResult
import dev.daimonos.remote.session.SessionReducer
import dev.daimonos.remote.session.SessionViewState
import java.util.concurrent.TimeUnit
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient

enum class AppMode {
    LOADING,
    PAIRING,
    WAITING_FOR_LOCAL_APPROVAL,
    CONNECTING,
    SESSION,
}

data class ControllerUiState(
    val mode: AppMode = AppMode.LOADING,
    val connected: Boolean = false,
    val pairingFingerprint: String? = null,
    val error: String? = null,
    val session: SessionViewState = SessionViewState(),
    val sessions: List<dev.daimonos.remote.protocol.SessionListEntry> = emptyList(),
    val grantedCapabilities: Set<ClientCapability> = emptySet(),
    val promptDraft: String = "",
    val promptPending: Boolean = false,
)

class ControllerViewModel(application: Application) : AndroidViewModel(application) {
    private val secureStore = SecureDeviceStore(application)
    private val httpClient = OkHttpClient.Builder()
        .pingInterval(20, TimeUnit.SECONDS)
        .build()
    private val remoteClient = RemoteSessionClient(httpClient)
    private var reducer = SessionReducer()
    private var daemon: PairedDaemon? = null
    private var activeConnection: SessionConnection? = null
    private var pairingJob: Job? = null
    private var sessionJob: Job? = null
    private var desiredSessionId: String? = null
    private var pendingPrompt: PendingPrompt? = null
    private var draftRevision: Long = 0
    private var replayTargetSeq: Long? = null

    private val mutableState = MutableStateFlow(ControllerUiState())
    val state: StateFlow<ControllerUiState> = mutableState.asStateFlow()

    init {
        viewModelScope.launch {
            try {
                daemon = secureStore.load()
            } catch (error: Throwable) {
                if (error is CancellationException) throw error
                val reset = try {
                    secureStore.resetIdentity()
                    true
                } catch (resetError: Throwable) {
                    if (resetError is CancellationException) throw resetError
                    false
                }
                mutableState.value = ControllerUiState(
                    mode = AppMode.PAIRING,
                    error = if (reset) {
                        "Secure storage was invalid and has been reset"
                    } else {
                        "Secure storage is unavailable"
                    },
                )
                return@launch
            }
            if (daemon == null) {
                mutableState.value = ControllerUiState(mode = AppMode.PAIRING)
            } else {
                startSessionLoop()
            }
        }
    }

    fun pair(endpoint: String, claim: String, label: String) {
        if (endpoint.isBlank() || claim.isBlank() || label.isBlank()) return
        pairingJob?.cancel()
        sessionJob?.cancel()
        pairingJob = viewModelScope.launch {
            mutableState.value = ControllerUiState(
                mode = AppMode.WAITING_FOR_LOCAL_APPROVAL,
            )
            try {
                val identity = loadIdentityOrReset() ?: return@launch
                val paired = remoteClient.pair(
                    endpoint = endpoint.trim(),
                    claim = claim.trim(),
                    label = label.trim(),
                    identity = identity,
                    capabilities = DEFAULT_CAPABILITIES,
                ) { pending ->
                    mutableState.value = mutableState.value.copy(
                        pairingFingerprint = pending.pairing.fingerprint,
                    )
                }
                secureStore.save(paired)
                daemon = paired
                reducer = SessionReducer()
                desiredSessionId = null
                startSessionLoop()
            } catch (error: Throwable) {
                if (error is CancellationException) throw error
                mutableState.value = ControllerUiState(
                    mode = AppMode.PAIRING,
                    error = error.message ?: "Pairing failed",
                )
            }
        }
    }

    fun sendPrompt(text: String): Boolean {
        if (text.isBlank() || pendingPrompt != null || mutableState.value.promptPending) {
            return false
        }
        val requestId = activeConnection?.prompt(text.trim())
        if (requestId == null) {
            showSendFailure()
            return false
        }
        pendingPrompt = PendingPrompt(
            requestId = requestId,
            draftRevision = draftRevision,
            text = text.trim(),
            sentAtSeq = reducer.state.seq,
        )
        mutableState.value = mutableState.value.copy(promptPending = true)
        return true
    }

    fun updatePromptDraft(text: String) {
        draftRevision += 1
        mutableState.value = mutableState.value.copy(promptDraft = text)
    }

    fun interrupt() {
        if (!send(SessionConnection::interrupt)) showSendFailure()
    }

    fun answerApproval(id: String, decision: ApprovalDecision) {
        if (!send { answerApproval(id, decision) }) showSendFailure()
    }

    fun switchSession(sessionId: String?) {
        desiredSessionId = sessionId
        reducer = SessionReducer()
        pendingPrompt = null
        replayTargetSeq = null
        mutableState.value = mutableState.value.copy(
            session = reducer.state,
            promptPending = false,
        )
        activeConnection?.close()
    }

    fun stopSession() {
        if (!send(SessionConnection::stopSession)) showSendFailure()
    }

    fun forgetDevice() {
        sessionJob?.cancel()
        pairingJob?.cancel()
        activeConnection?.close()
        activeConnection = null
        viewModelScope.launch {
            secureStore.clear()
            daemon = null
            reducer = SessionReducer()
            mutableState.value = ControllerUiState(mode = AppMode.PAIRING)
        }
    }

    private fun startSessionLoop() {
        sessionJob?.cancel()
        sessionJob = viewModelScope.launch {
            var retryDelay = 500L
            while (currentCoroutineContext().isActive && daemon != null) {
                mutableState.value = mutableState.value.copy(
                    mode = AppMode.CONNECTING,
                    connected = false,
                )
                try {
                    val identity = loadIdentityOrReset() ?: return@launch
                    val retained = reducer.state
                    val targetSession = desiredSessionId ?: retained.sessionId
                    val connection = remoteClient.connect(
                        daemon = checkNotNull(daemon),
                        identity = identity,
                        clientLabel = android.os.Build.MODEL.take(120),
                        sessionId = targetSession,
                        lastSeenSeq = retained.sessionId
                            ?.takeIf { it == targetSession }
                            ?.let { retained.seq },
                    )
                    activeConnection = connection
                    retryDelay = 500L
                    receiveSession(connection)
                } catch (error: Throwable) {
                    if (error is CancellationException) throw error
                    if (error is RemoteAuthenticationException) {
                        secureStore.clear()
                        daemon = null
                        reducer = SessionReducer()
                        mutableState.value = ControllerUiState(
                            mode = AppMode.PAIRING,
                            error = "Stored device access is no longer valid; pair again",
                        )
                        return@launch
                    }
                    if (error is RemoteAttachDeniedException) {
                        secureStore.clear()
                        daemon = null
                        reducer = SessionReducer()
                        mutableState.value = ControllerUiState(
                            mode = AppMode.PAIRING,
                            error = "Session attachment was denied: ${error.message}",
                        )
                        return@launch
                    }
                    mutableState.value = mutableState.value.copy(
                        connected = false,
                        error = error.message ?: "Connection failed",
                    )
                } finally {
                    activeConnection?.close()
                    activeConnection = null
                }
                delay(retryDelay)
                retryDelay = (retryDelay * 2).coerceAtMost(10_000L)
            }
        }
    }

    private suspend fun receiveSession(connection: SessionConnection) {
        while (currentCoroutineContext().isActive) {
            when (val message = connection.receive()) {
                is ServerMessage.AttachOk -> {
                    check(message.protocolVersion == dev.daimonos.remote.protocol.PROTOCOL_VERSION)
                    desiredSessionId = message.sessionId
                    replayTargetSeq = message.seq
                    pendingPrompt?.takeIf { message.seq <= it.sentAtSeq }?.let {
                        releasePendingPrompt(accepted = false)
                    }
                    mutableState.value = mutableState.value.copy(
                        mode = AppMode.SESSION,
                        connected = true,
                        error = null,
                        grantedCapabilities = message.grantedCapabilities.toSet(),
                    )
                    mutableState.value = mutableState.value.copy(sessions = emptyList())
                    connection.listSessions()
                }
                is ServerMessage.Snapshot -> {
                    check(message.seq == message.state.seq)
                    reducer.applySnapshot(message.state)
                    reconcilePendingPromptFromSnapshot(message.state)
                    replayTargetSeq = null
                    publishSession()
                }
                is ServerMessage.Event -> {
                    val pending = pendingPrompt
                    if (
                        pending != null &&
                        message.event is dev.daimonos.remote.protocol.SessionEvent.UserMessage &&
                        message.event.requestId == pending.requestId
                    ) {
                        releasePendingPrompt(accepted = true)
                    }
                    when (reducer.applyEvent(message.seq, message.event)) {
                        ApplyResult.Applied -> publishSession()
                        ApplyResult.Duplicate -> Unit
                        is ApplyResult.Gap -> connection.sync(reducer.state.seq)
                    }
                    if (
                        pendingPrompt != null &&
                        replayTargetSeq?.let { message.seq >= it } == true
                    ) {
                        releasePendingPrompt(accepted = false)
                    }
                    if (replayTargetSeq?.let { message.seq >= it } == true) {
                        replayTargetSeq = null
                    }
                }
                is ServerMessage.SessionList -> {
                    mutableState.value = mutableState.value.copy(
                        sessions = (
                            mutableState.value.sessions + message.sessions
                        ).distinctBy { it.sessionId },
                    )
                    message.nextCursor?.let(connection::listSessions)
                }
                is ServerMessage.Revoked -> {
                    if (
                        reducer.state.endingReason != null ||
                        message.code == RevocationCode.SESSION_STOPPED ||
                        (
                            message.code == null &&
                                message.reason.contains("stopped", ignoreCase = true)
                        )
                    ) {
                        desiredSessionId = null
                        reducer = SessionReducer()
                        releasePendingPrompt(accepted = false)
                    }
                    mutableState.value = mutableState.value.copy(
                        connected = false,
                        error = message.reason,
                    )
                    return
                }
                is ServerMessage.AttachDenied -> {
                    if (desiredSessionId != null || reducer.state.sessionId != null) {
                        desiredSessionId = null
                        reducer = SessionReducer()
                        error(message.reason)
                    }
                    throw RemoteAttachDeniedException(message.reason)
                }
                is ServerMessage.Error -> {
                    if (message.requestId == pendingPrompt?.requestId) {
                        releasePendingPrompt(accepted = false)
                    }
                    mutableState.value = mutableState.value.copy(
                        error = "${message.code}: ${message.message}",
                    )
                }
                is ServerMessage.CommandResult,
                is ServerMessage.Usage,
                ServerMessage.Pong,
                -> Unit
            }
        }
    }

    private fun publishSession() {
        mutableState.value = mutableState.value.copy(
            mode = AppMode.SESSION,
            connected = true,
            session = reducer.state,
        )
    }

    private fun reconcilePendingPromptFromSnapshot(
        snapshot: dev.daimonos.remote.protocol.SessionSnapshot,
    ) {
        val pending = pendingPrompt ?: return
        if (snapshot.seq < pending.sentAtSeq) return
        val accepted = snapshot.seq > pending.sentAtSeq &&
            snapshot.transcript
                .lastOrNull { it.role == dev.daimonos.remote.protocol.TranscriptRole.USER }
                ?.text == pending.text
        releasePendingPrompt(accepted)
    }

    private fun releasePendingPrompt(accepted: Boolean) {
        val pending = pendingPrompt ?: return
        pendingPrompt = null
        mutableState.value = mutableState.value.copy(
            promptDraft = if (accepted && draftRevision == pending.draftRevision) {
                ""
            } else {
                mutableState.value.promptDraft
            },
            promptPending = false,
        )
    }

    private fun send(block: SessionConnection.() -> Boolean): Boolean =
        activeConnection?.block() == true

    private fun showSendFailure() {
        mutableState.value = mutableState.value.copy(
            error = "Command could not be queued",
        )
    }

    private suspend fun loadIdentityOrReset(): DeviceIdentity? = try {
        secureStore.getOrCreateIdentity()
    } catch (error: Throwable) {
        if (error is CancellationException) throw error
        val resetError = try {
            secureStore.resetIdentity()
            null
        } catch (resetFailure: Throwable) {
            if (resetFailure is CancellationException) throw resetFailure
            resetFailure
        }
        daemon = null
        reducer = SessionReducer()
        mutableState.value = ControllerUiState(
            mode = AppMode.PAIRING,
            error = if (resetError == null) {
                "Secure device identity was invalid and has been reset"
            } else {
                "Secure storage is unavailable: ${resetError.message}"
            },
        )
        null
    }

    override fun onCleared() {
        pairingJob?.cancel()
        sessionJob?.cancel()
        activeConnection?.close()
        httpClient.dispatcher.executorService.shutdown()
        httpClient.connectionPool.evictAll()
        super.onCleared()
    }

    private companion object {
        val DEFAULT_CAPABILITIES = listOf(
            ClientCapability.OBSERVE,
            ClientCapability.PROMPT,
            ClientCapability.CONFIGURE,
            ClientCapability.INTERRUPT,
            ClientCapability.STOP,
            ClientCapability.APPROVE_ONCE,
            ClientCapability.APPROVE_ALWAYS,
        )
    }

    private data class PendingPrompt(
        val requestId: String,
        val draftRevision: Long,
        val text: String,
        val sentAtSeq: Long,
    )

    private class RemoteAttachDeniedException(message: String) :
        IllegalStateException(message)
}
