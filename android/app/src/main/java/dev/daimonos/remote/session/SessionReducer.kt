package dev.daimonos.remote.session

import dev.daimonos.remote.protocol.ActiveToolState
import dev.daimonos.remote.protocol.ApprovalRequest
import dev.daimonos.remote.protocol.AssistantOutcome
import dev.daimonos.remote.protocol.ContextUsage
import dev.daimonos.remote.protocol.DurabilityStatus
import dev.daimonos.remote.protocol.HistoryWindow
import dev.daimonos.remote.protocol.RuntimeOption
import dev.daimonos.remote.protocol.SessionEvent
import dev.daimonos.remote.protocol.SessionSnapshot
import dev.daimonos.remote.protocol.TimelineEntry
import dev.daimonos.remote.protocol.ToolCallStatus
import dev.daimonos.remote.protocol.TurnStatus

data class SessionViewState(
    val sessionId: String? = null,
    val seq: Long = 0,
    val turnStatus: TurnStatus = TurnStatus.IDLE,
    val durabilityStatus: DurabilityStatus = DurabilityStatus.SAVED,
    val timeline: List<TimelineEntry> = emptyList(),
    val activeTools: List<ActiveToolState> = emptyList(),
    val historyWindow: HistoryWindow = HistoryWindow(0, 0, 0),
    val pendingApprovals: List<ApprovalRequest> = emptyList(),
    val runtimeOptions: List<RuntimeOption> = emptyList(),
    val contextUsage: ContextUsage? = null,
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

    var state: SessionViewState = trim(initial)
        private set
    private var nextEntryId: Long =
        (state.timeline.maxOfOrNull { maxOf(it.id, it.order) } ?: 0) + 1
    private var openEntryId: Long? = null

    fun applySnapshot(snapshot: SessionSnapshot) {
        nextEntryId = (
            snapshot.timeline.maxOfOrNull { maxOf(it.id, it.order) }
                ?: snapshot.activeTools.maxOfOrNull(ActiveToolState::occurrenceId)
                ?: 0
            ) + 1
        openEntryId = null
        state = trim(
            SessionViewState(
                sessionId = snapshot.sessionId,
                seq = snapshot.seq,
                turnStatus = snapshot.turnStatus,
                durabilityStatus = snapshot.durabilityStatus,
                timeline = snapshot.timeline,
                activeTools = snapshot.activeTools,
                historyWindow = snapshot.historyWindow,
                pendingApprovals = snapshot.pendingApprovals,
                runtimeOptions = snapshot.runtimeOptions,
                contextUsage = snapshot.contextUsage,
            ),
        )
    }

    fun applyEvent(seq: Long, event: SessionEvent): ApplyResult {
        if (seq <= state.seq) return ApplyResult.Duplicate
        val expected = state.seq + 1
        if (seq != expected) return ApplyResult.Gap(expected, seq)
        state = trim(reduce(state, seq, event))
        return ApplyResult.Applied
    }

    private fun reduce(previous: SessionViewState, seq: Long, event: SessionEvent): SessionViewState =
        when (event) {
            is SessionEvent.UserMessage -> {
                closeOpen()
                val id = mintId()
                previous.withEntry(seq, TimelineEntry.User(id, id, event.text))
            }
            is SessionEvent.AssistantDelta -> previous.appendDelta(seq, event.text, thought = false)
            is SessionEvent.ThoughtDelta -> previous.appendDelta(seq, event.text, thought = true)
            is SessionEvent.AssistantDone -> {
                closeOpen()
                val id = mintId()
                previous.withEntry(seq, TimelineEntry.Outcome(id, id, event.outcome))
            }
            is SessionEvent.ToolCallStarted -> {
                closeOpen()
                val occurrenceId = mintId()
                previous.copy(
                    seq = seq,
                    timeline = previous.timeline + TimelineEntry.Tool(
                        id = occurrenceId,
                        order = occurrenceId,
                        toolCallId = event.id,
                        name = event.name,
                        title = event.title,
                        status = ToolCallStatus.PENDING,
                    ),
                    activeTools = previous.activeTools + ActiveToolState(
                            occurrenceId = occurrenceId,
                            toolCallId = event.id,
                            name = event.name,
                            title = event.title,
                            status = ToolCallStatus.PENDING,
                        ),
                )
            }
            is SessionEvent.ToolCallUpdated ->
                previous.updateTool(seq, event.id, event.status, null)
            is SessionEvent.ToolCallFinished ->
                previous.updateTool(seq, event.id, event.status, event.output)
            is SessionEvent.ApprovalRequested -> previous.copy(
                seq = seq,
                pendingApprovals = previous.pendingApprovals
                    .filterNot { it.id == event.request.id } + event.request,
            )
            is SessionEvent.ApprovalResolved -> previous.copy(
                seq = seq,
                pendingApprovals = previous.pendingApprovals.filterNot { it.id == event.approvalId },
            )
            is SessionEvent.ApprovalDeadlineChanged -> previous.copy(
                seq = seq,
                pendingApprovals = previous.pendingApprovals.map { approval ->
                    if (approval.id == event.approvalId) {
                        approval.copy(
                            ineligibleDeadlineUnixMs = event.ineligibleDeadlineUnixMs,
                            deadlinePaused = event.paused,
                        )
                    } else {
                        approval
                    }
                },
            )
            is SessionEvent.RuntimeOptionsChanged -> previous.copy(seq = seq, runtimeOptions = event.options)
            is SessionEvent.ContextUsageChanged -> previous.copy(seq = seq, contextUsage = event.usage)
            SessionEvent.ConversationCleared -> {
                closeOpen()
                previous.copy(
                    seq = seq,
                    timeline = emptyList(),
                    activeTools = emptyList(),
                    historyWindow = HistoryWindow(0, 0, 0),
                    pendingApprovals = emptyList(),
                    endingReason = null,
                )
            }
            is SessionEvent.TurnStatusChanged -> previous.copy(seq = seq, turnStatus = event.status)
            is SessionEvent.DurabilityStatusChanged ->
                previous.copy(seq = seq, durabilityStatus = event.status)
            is SessionEvent.SessionEnding -> {
                closeOpen()
                previous.copy(seq = seq, endingReason = event.reason)
            }
        }

    private fun SessionViewState.appendDelta(
        seq: Long,
        text: String,
        thought: Boolean,
    ): SessionViewState {
        val last = timeline.lastOrNull()
        val updated = when {
            last?.id != openEntryId -> null
            thought && last is TimelineEntry.Thought -> last.copy(text = last.text + text)
            !thought && last is TimelineEntry.Assistant -> last.copy(text = last.text + text)
            else -> null
        }
        if (updated != null) {
            return copy(seq = seq, timeline = timeline.dropLast(1) + updated)
        }
        closeOpen()
        val id = mintId()
        openEntryId = id
        val entry = if (thought) {
            TimelineEntry.Thought(id, id, text)
        } else {
            TimelineEntry.Assistant(id, id, text)
        }
        return withEntry(seq, entry)
    }

    private fun SessionViewState.updateTool(
        seq: Long,
        toolCallId: String,
        status: ToolCallStatus,
        output: String?,
    ): SessionViewState {
        val occurrenceId = timeline.asReversed().firstNotNullOfOrNull { entry ->
            (entry as? TimelineEntry.Tool)
                ?.takeIf { it.toolCallId == toolCallId && !it.status.isTerminal() }
                ?.id
        } ?: activeTools.asReversed()
            .firstOrNull { it.toolCallId == toolCallId }
            ?.occurrenceId
        if (occurrenceId == null) return copy(seq = seq)
        return copy(
            seq = seq,
            timeline = timeline.map { entry ->
                if (entry is TimelineEntry.Tool && entry.id == occurrenceId) {
                    entry.copy(status = status, output = output ?: entry.output)
                } else {
                    entry
                }
            },
            activeTools = if (status.isTerminal()) {
                activeTools.filterNot { it.occurrenceId == occurrenceId }
            } else {
                activeTools.map {
                    if (it.occurrenceId == occurrenceId) it.copy(status = status) else it
                }
            },
        )
    }

    private fun SessionViewState.withEntry(seq: Long, entry: TimelineEntry): SessionViewState =
        copy(seq = seq, timeline = timeline + entry)

    private fun trim(value: SessionViewState): SessionViewState {
        val excess = (value.timeline.size - maxEntries).coerceAtLeast(0)
        val timeline = value.timeline.drop(excess)
        val truncatedBefore = value.historyWindow.truncatedBefore + excess
        return value.copy(
            timeline = timeline,
            historyWindow = value.historyWindow.copy(
                truncatedBefore = truncatedBefore,
                retained = timeline.size,
                total = truncatedBefore + timeline.size,
            ),
        )
    }

    private fun mintId(): Long = nextEntryId.also { nextEntryId += 1 }

    private fun closeOpen() {
        openEntryId = null
    }
}

private fun ToolCallStatus.isTerminal(): Boolean =
    this == ToolCallStatus.COMPLETED ||
        this == ToolCallStatus.FAILED ||
        this == ToolCallStatus.CANCELLED

fun AssistantOutcome.displayText(): String = when (this) {
    AssistantOutcome.Completed -> "Turn completed"
    is AssistantOutcome.Errored -> message
    AssistantOutcome.Refused -> "The model refused the request"
    AssistantOutcome.Aborted -> "The turn was aborted"
    AssistantOutcome.MaxTokens -> "The model reached its output-token limit"
}
