package dev.daimonos.remote.session

import dev.daimonos.remote.protocol.ApprovalRequest
import dev.daimonos.remote.protocol.AssistantOutcome
import dev.daimonos.remote.protocol.ContextUsage
import dev.daimonos.remote.protocol.RuntimeOption
import dev.daimonos.remote.protocol.SessionEvent
import dev.daimonos.remote.protocol.SessionSnapshot
import dev.daimonos.remote.protocol.ToolCallState
import dev.daimonos.remote.protocol.ToolCallStatus
import dev.daimonos.remote.protocol.TranscriptEntry
import dev.daimonos.remote.protocol.TranscriptRole
import dev.daimonos.remote.protocol.TurnStatus

data class SessionViewState(
    val sessionId: String? = null,
    val seq: Long = 0,
    val turnStatus: TurnStatus = TurnStatus.IDLE,
    val transcript: List<TranscriptEntry> = emptyList(),
    val toolCalls: List<ToolCallState> = emptyList(),
    val pendingApprovals: List<ApprovalRequest> = emptyList(),
    val runtimeOptions: List<RuntimeOption> = emptyList(),
    val contextUsage: ContextUsage? = null,
    val historyTruncated: Boolean = false,
    val endingReason: String? = null,
)

sealed interface ApplyResult {
    data object Applied : ApplyResult
    data object Duplicate : ApplyResult
    data class Gap(val expected: Long, val received: Long) : ApplyResult
}

class SessionReducer(
    initial: SessionViewState = SessionViewState(),
    private val maxEntries: Int = 2_000,
) {
    init {
        require(maxEntries > 0)
    }
    var state: SessionViewState = initial
        private set

    fun applySnapshot(snapshot: SessionSnapshot) {
        state = SessionViewState(
            sessionId = snapshot.sessionId,
            seq = snapshot.seq,
            turnStatus = snapshot.turnStatus,
            transcript = snapshot.transcript.takeLast(maxEntries),
            toolCalls = snapshot.toolCalls.takeLast(maxEntries),
            pendingApprovals = snapshot.pendingApprovals,
            runtimeOptions = snapshot.runtimeOptions,
            contextUsage = snapshot.contextUsage,
            historyTruncated = snapshot.historyTruncated ||
                snapshot.transcript.size > maxEntries ||
                snapshot.toolCalls.size > maxEntries,
        )
    }

    fun applyEvent(seq: Long, event: SessionEvent): ApplyResult {
        if (seq <= state.seq) return ApplyResult.Duplicate
        val expected = state.seq + 1
        if (seq != expected) return ApplyResult.Gap(expected, seq)
        state = reduce(state, seq, event)
        return ApplyResult.Applied
    }

    private fun reduce(
        previous: SessionViewState,
        seq: Long,
        event: SessionEvent,
    ): SessionViewState {
        val reduced = when (event) {
        is SessionEvent.UserMessage -> previous.copy(
            seq = seq,
            transcript = previous.transcript + TranscriptEntry(
                id = previous.nextTranscriptId(),
                role = TranscriptRole.USER,
                text = event.text,
            ),
        )
        is SessionEvent.AssistantDelta -> previous.copy(
            seq = seq,
            transcript = previous.transcript.appendDelta(
                role = TranscriptRole.ASSISTANT,
                id = previous.nextTranscriptId(),
                text = event.text,
            ),
        )
        is SessionEvent.ThoughtDelta -> previous.copy(
            seq = seq,
            transcript = previous.transcript.appendDelta(
                role = TranscriptRole.THOUGHT,
                id = previous.nextTranscriptId(),
                text = event.text,
            ),
        )
        is SessionEvent.AssistantDone -> previous.copy(
            seq = seq,
            transcript = previous.transcript.finishAssistant(
                event.outcome,
                previous.nextTranscriptId(),
            ),
        )
        is SessionEvent.ToolCallStarted -> previous.copy(
            seq = seq,
            toolCalls = previous.toolCalls.upsert(
                ToolCallState(
                    id = event.id,
                    name = event.name,
                    title = event.title,
                    status = ToolCallStatus.PENDING,
                ),
            ),
        )
        is SessionEvent.ToolCallUpdated -> previous.copy(
            seq = seq,
            toolCalls = previous.toolCalls.update(event.id) {
                it.copy(status = event.status)
            },
        )
        is SessionEvent.ToolCallFinished -> previous.copy(
            seq = seq,
            toolCalls = previous.toolCalls.update(event.id) {
                it.copy(status = event.status, output = event.output)
            },
        )
        is SessionEvent.ApprovalRequested -> previous.copy(
            seq = seq,
            pendingApprovals = previous.pendingApprovals
                .filterNot { it.id == event.request.id } + event.request,
        )
        is SessionEvent.ApprovalResolved -> previous.copy(
            seq = seq,
            pendingApprovals = previous.pendingApprovals
                .filterNot { it.id == event.approvalId },
        )
        is SessionEvent.RuntimeOptionsChanged -> previous.copy(
            seq = seq,
            runtimeOptions = event.options,
        )
        is SessionEvent.ContextUsageChanged -> previous.copy(
            seq = seq,
            contextUsage = event.usage,
        )
        SessionEvent.ConversationCleared -> previous.copy(
            seq = seq,
            transcript = emptyList(),
            toolCalls = emptyList(),
            pendingApprovals = emptyList(),
            historyTruncated = false,
            endingReason = null,
        )
        is SessionEvent.TurnStatusChanged -> previous.copy(
            seq = seq,
            turnStatus = event.status,
        )
        is SessionEvent.SessionEnding -> previous.copy(
            seq = seq,
            endingReason = event.reason,
        )
        }
        return reduced.copy(
            transcript = reduced.transcript.takeLast(maxEntries),
            toolCalls = reduced.toolCalls.takeLast(maxEntries),
            historyTruncated = reduced.historyTruncated ||
                reduced.transcript.size > maxEntries ||
                reduced.toolCalls.size > maxEntries,
        )
    }
}

private fun SessionViewState.nextTranscriptId(): Long =
    (transcript.maxOfOrNull(TranscriptEntry::id) ?: 0) + 1

private fun List<TranscriptEntry>.appendDelta(
    role: TranscriptRole,
    id: Long,
    text: String,
): List<TranscriptEntry> {
    val last = lastOrNull()
    return if (last?.role == role && last.outcome == null) {
        dropLast(1) + last.copy(text = last.text + text)
    } else {
        this + TranscriptEntry(id = id, role = role, text = text)
    }
}

private fun List<TranscriptEntry>.finishAssistant(
    outcome: AssistantOutcome,
    id: Long,
): List<TranscriptEntry> {
    val index = indexOfLast { it.role == TranscriptRole.ASSISTANT && it.outcome == null }
    if (index < 0) {
        if (outcome == AssistantOutcome.Completed) return this
        return this + TranscriptEntry(
            id = id,
            role = TranscriptRole.SYSTEM,
            text = outcome.displayText(),
        )
    }
    return mapIndexed { entryIndex, entry ->
        if (entryIndex == index) entry.copy(outcome = outcome) else entry
    }
}

fun AssistantOutcome.displayText(): String = when (this) {
    AssistantOutcome.Completed -> "Turn completed"
    is AssistantOutcome.Errored -> message
    AssistantOutcome.Refused -> "The model refused the request"
    AssistantOutcome.Aborted -> "The turn was aborted"
    AssistantOutcome.MaxTokens -> "The model reached its output-token limit"
}

private fun List<ToolCallState>.upsert(call: ToolCallState): List<ToolCallState> =
    filterNot { it.id == call.id } + call

private fun List<ToolCallState>.update(
    id: String,
    transform: (ToolCallState) -> ToolCallState,
): List<ToolCallState> = map { call ->
    if (call.id == id) transform(call) else call
}
