package dev.daimonos.remote.session

import dev.daimonos.remote.protocol.ActiveToolState
import dev.daimonos.remote.protocol.ApprovalRequest
import dev.daimonos.remote.protocol.AssistantOutcome
import dev.daimonos.remote.protocol.HistoryWindow
import dev.daimonos.remote.protocol.SessionEvent
import dev.daimonos.remote.protocol.SessionSnapshot
import dev.daimonos.remote.protocol.TimelineEntry
import dev.daimonos.remote.protocol.ToolCallStatus
import dev.daimonos.remote.protocol.TurnStatus
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionReducerTest {
    @Test
    fun toolStartClosesAssistantAndPreservesInterleaving() {
        val reducer = SessionReducer()
        reducer.applyEvent(1, SessionEvent.AssistantDelta("before"))
        reducer.applyEvent(2, SessionEvent.ToolCallStarted("call", "read", "read"))
        reducer.applyEvent(3, SessionEvent.AssistantDelta("after"))

        assertTrue(reducer.state.timeline[0] is TimelineEntry.Assistant)
        assertTrue(reducer.state.timeline[1] is TimelineEntry.Tool)
        assertTrue(reducer.state.timeline[2] is TimelineEntry.Assistant)
    }

    @Test
    fun outcomeIsItsOwnTimelineEntry() {
        val reducer = SessionReducer()
        reducer.applyEvent(1, SessionEvent.AssistantDelta("hello "))
        reducer.applyEvent(2, SessionEvent.AssistantDelta("world"))
        reducer.applyEvent(3, SessionEvent.AssistantDone(AssistantOutcome.Completed))

        assertEquals("hello world", (reducer.state.timeline[0] as TimelineEntry.Assistant).text)
        assertEquals(
            AssistantOutcome.Completed,
            (reducer.state.timeline[1] as TimelineEntry.Outcome).outcome,
        )
    }

    @Test
    fun reusedProviderIdUpdatesNewestNonterminalOccurrence() {
        val reducer = SessionReducer()
        reducer.applyEvent(1, SessionEvent.ToolCallStarted("call_0", "exec", "first"))
        reducer.applyEvent(2, SessionEvent.ToolCallStarted("call_0", "exec", "second"))

        assertEquals(2, reducer.state.activeTools.size)
        reducer.applyEvent(
            3,
            SessionEvent.ToolCallFinished("call_0", ToolCallStatus.COMPLETED, "second output"),
        )
        var calls = reducer.state.timeline.filterIsInstance<TimelineEntry.Tool>()
        assertEquals(ToolCallStatus.PENDING, calls[0].status)
        assertEquals(ToolCallStatus.COMPLETED, calls[1].status)
        assertEquals(1, reducer.state.activeTools.size)

        reducer.applyEvent(
            4,
            SessionEvent.ToolCallFinished("call_0", ToolCallStatus.COMPLETED, "first output"),
        )

        calls = reducer.state.timeline.filterIsInstance<TimelineEntry.Tool>()
        assertTrue(calls[0].id != calls[1].id)
        assertEquals("first output", calls[0].output)
        assertEquals("second output", calls[1].output)
        assertTrue(reducer.state.activeTools.isEmpty())
    }

    @Test
    fun activeToolSurvivesLocalHistoryTrimAndTerminalizesOffWindow() {
        val reducer = SessionReducer(maxEntries = 1)
        reducer.applyEvent(1, SessionEvent.ToolCallStarted("long", "exec", "long"))
        reducer.applyEvent(2, SessionEvent.UserMessage("new"))

        assertEquals("long", reducer.state.activeTools.single().toolCallId)
        assertEquals(1, reducer.state.historyWindow.truncatedBefore)
        reducer.applyEvent(
            3,
            SessionEvent.ToolCallFinished("long", ToolCallStatus.COMPLETED, "done"),
        )
        assertTrue(reducer.state.activeTools.isEmpty())
    }

    @Test
    fun duplicateIsIgnoredAndGapDoesNotMutateState() {
        val reducer = SessionReducer()
        reducer.applyEvent(1, SessionEvent.UserMessage("one"))
        assertEquals(ApplyResult.Duplicate, reducer.applyEvent(1, SessionEvent.UserMessage("x")))
        assertEquals(
            ApplyResult.Gap(expected = 2, received = 3),
            reducer.applyEvent(3, SessionEvent.UserMessage("gap")),
        )
        assertEquals("one", (reducer.state.timeline.single() as TimelineEntry.User).text)
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
            SessionEvent.ApprovalDeadlineChanged(first.id, 123_456, true),
        )
        reducer.applyEvent(
            4,
            SessionEvent.ApprovalResolved(
                first.id,
                dev.daimonos.remote.protocol.ApprovalDecision.DENY,
                "host",
            ),
        )
        assertEquals(listOf(second), reducer.state.pendingApprovals)
    }

    @Test
    fun snapshotTrimLayersHistoryWindowAccurately() {
        val reducer = SessionReducer(maxEntries = 1)
        reducer.applySnapshot(
            SessionSnapshot(
                sessionId = "session",
                seq = 2,
                turnStatus = TurnStatus.IDLE,
                timeline = listOf(
                    TimelineEntry.User(1, 1, "one"),
                    TimelineEntry.User(2, 2, "two"),
                ),
                activeTools = listOf(
                    ActiveToolState(3, "long", "exec", "long", ToolCallStatus.IN_PROGRESS),
                ),
                historyWindow = HistoryWindow(4, 2, 6),
                pendingApprovals = emptyList(),
                runtimeOptions = emptyList(),
            ),
        )
        assertEquals(5, reducer.state.historyWindow.truncatedBefore)
        assertEquals(1, reducer.state.historyWindow.retained)
        assertEquals(6L, reducer.state.historyWindow.total)
        assertEquals("long", reducer.state.activeTools.single().toolCallId)
    }

    @Test
    fun canonicalClearResetsTimelineWindow() {
        val reducer = SessionReducer(maxEntries = 1)
        reducer.applyEvent(1, SessionEvent.UserMessage("before"))
        reducer.applyEvent(2, SessionEvent.UserMessage("trim"))
        reducer.applyEvent(3, SessionEvent.ConversationCleared)
        assertTrue(reducer.state.timeline.isEmpty())
        assertEquals(0, reducer.state.historyWindow.truncatedBefore)
        assertEquals(3, reducer.state.seq)
    }

    private fun approval(id: String) = ApprovalRequest(
        id = id,
        toolCallId = "tool-$id",
        tool = "exec",
        detail = "command",
        allowAlwaysAvailable = false,
    )
}
