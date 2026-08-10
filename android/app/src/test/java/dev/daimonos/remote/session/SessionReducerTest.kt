package dev.daimonos.remote.session

import dev.daimonos.remote.protocol.ApprovalRequest
import dev.daimonos.remote.protocol.AssistantOutcome
import dev.daimonos.remote.protocol.SessionEvent
import dev.daimonos.remote.protocol.SessionSnapshot
import dev.daimonos.remote.protocol.TranscriptRole
import dev.daimonos.remote.protocol.TranscriptEntry
import dev.daimonos.remote.protocol.TurnStatus
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionReducerTest {
    @Test
    fun deltasCoalesceAndTerminalOutcomeFinishesAssistantEntry() {
        val reducer = SessionReducer()

        assertEquals(
            ApplyResult.Applied,
            reducer.applyEvent(1, SessionEvent.AssistantDelta("hello ")),
        )
        reducer.applyEvent(2, SessionEvent.AssistantDelta("world"))
        reducer.applyEvent(
            3,
            SessionEvent.AssistantDone(AssistantOutcome.Completed),
        )

        val entry = reducer.state.transcript.single()
        assertEquals(TranscriptRole.ASSISTANT, entry.role)
        assertEquals("hello world", entry.text)
        assertEquals(AssistantOutcome.Completed, entry.outcome)
    }

    @Test
    fun duplicateIsIgnoredAndGapDoesNotMutateState() {
        val reducer = SessionReducer()
        reducer.applyEvent(1, SessionEvent.UserMessage("one"))

        assertEquals(
            ApplyResult.Duplicate,
            reducer.applyEvent(1, SessionEvent.UserMessage("duplicate")),
        )
        assertEquals(
            ApplyResult.Gap(expected = 2, received = 3),
            reducer.applyEvent(3, SessionEvent.UserMessage("gap")),
        )
        assertEquals(listOf("one"), reducer.state.transcript.map { it.text })
    }

    @Test
    fun approvalResolutionRemovesOnlyMatchingRequest() {
        val reducer = SessionReducer()
        val first = approval("first")
        val second = approval("second")
        reducer.applyEvent(1, SessionEvent.ApprovalRequested(first))
        reducer.applyEvent(2, SessionEvent.ApprovalRequested(second))
        reducer.applyEvent(
            3,
            SessionEvent.ApprovalResolved(
                approvalId = first.id,
                decision = dev.daimonos.remote.protocol.ApprovalDecision.DENY,
                resolvedBy = "host",
            ),
        )

        assertEquals(listOf(second), reducer.state.pendingApprovals)
        assertTrue(reducer.state.transcript.isEmpty())
    }

    @Test
    fun zeroTokenTerminalOutcomeRemainsVisible() {
        val reducer = SessionReducer()
        reducer.applyEvent(
            1,
            SessionEvent.AssistantDone(
                AssistantOutcome.Errored(
                    contextOverflow = false,
                    message = "provider unavailable",
                ),
            ),
        )

        assertEquals("provider unavailable", reducer.state.transcript.single().text)
        assertEquals(TranscriptRole.SYSTEM, reducer.state.transcript.single().role)
    }

    @Test
    fun zeroTokenCompletedOutcomeDoesNotCreateSyntheticRow() {
        val reducer = SessionReducer()
        reducer.applyEvent(1, SessionEvent.AssistantDone(AssistantOutcome.Completed))

        assertTrue(reducer.state.transcript.isEmpty())
    }

    @Test
    fun liveTranscriptIsBounded() {
        val reducer = SessionReducer(maxEntries = 2)
        reducer.applyEvent(1, SessionEvent.UserMessage("one"))
        reducer.applyEvent(2, SessionEvent.UserMessage("two"))
        reducer.applyEvent(3, SessionEvent.UserMessage("three"))

        assertEquals(listOf("two", "three"), reducer.state.transcript.map { it.text })
    }

    @Test
    fun oversizedSnapshotMarksHistoryTruncatedLocally() {
        val reducer = SessionReducer(maxEntries = 1)
        reducer.applySnapshot(
            SessionSnapshot(
                sessionId = "session",
                seq = 2,
                turnStatus = TurnStatus.IDLE,
                transcript = listOf(
                    TranscriptEntry(1, TranscriptRole.USER, "one"),
                    TranscriptEntry(2, TranscriptRole.USER, "two"),
                ),
                toolCalls = emptyList(),
                pendingApprovals = emptyList(),
                runtimeOptions = emptyList(),
            ),
        )

        assertTrue(reducer.state.historyTruncated)
        assertEquals(listOf("two"), reducer.state.transcript.map { it.text })
    }

    private fun approval(id: String) = ApprovalRequest(
        id = id,
        toolCallId = "tool-$id",
        tool = "exec",
        detail = "command",
        allowAlwaysAvailable = false,
    )
}
