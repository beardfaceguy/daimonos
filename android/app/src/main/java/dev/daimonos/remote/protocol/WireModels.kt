@file:OptIn(kotlinx.serialization.ExperimentalSerializationApi::class)

package dev.daimonos.remote.protocol

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.EncodeDefault
import kotlinx.serialization.KSerializer
import kotlinx.serialization.SerializationException
import kotlinx.serialization.descriptors.PrimitiveKind
import kotlinx.serialization.descriptors.PrimitiveSerialDescriptor
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.JsonClassDiscriminator
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonEncoder
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.longOrNull

const val PROTOCOL_VERSION: Int = 2

@Serializable
enum class ClientCapability {
    @SerialName("observe") OBSERVE,
    @SerialName("prompt") PROMPT,
    @SerialName("configure") CONFIGURE,
    @SerialName("interrupt") INTERRUPT,
    @SerialName("stop") STOP,
    @SerialName("approve_once") APPROVE_ONCE,
    @SerialName("approve_always") APPROVE_ALWAYS,
}

@Serializable
enum class ClientKind {
    @SerialName("terminal") TERMINAL,
    @SerialName("android") ANDROID,
    @SerialName("headless") HEADLESS,
    @SerialName("browser") BROWSER,
    @SerialName("agent") AGENT,
}

@Serializable
data class ClientInfo(
    val id: String,
    val kind: ClientKind,
    val label: String,
)

@Serializable
enum class ApprovalDecision {
    @SerialName("allow_once") ALLOW_ONCE,
    @SerialName("allow_always") ALLOW_ALWAYS,
    @SerialName("deny") DENY,
}

@Serializable
@JsonClassDiscriminator("type")
sealed interface ClientMessage {
    @Serializable
    @SerialName("attach")
    data class Attach(
        @EncodeDefault(EncodeDefault.Mode.ALWAYS)
        @SerialName("protocol_version") val protocolVersion: Int = PROTOCOL_VERSION,
        @SerialName("session_id") val sessionId: String? = null,
        val ticket: String?,
        val client: ClientInfo,
        @SerialName("requested_capabilities")
        val requestedCapabilities: List<ClientCapability>,
    ) : ClientMessage

    @Serializable
    @SerialName("resume")
    data class Resume(
        @EncodeDefault(EncodeDefault.Mode.ALWAYS)
        @SerialName("protocol_version") val protocolVersion: Int = PROTOCOL_VERSION,
        @SerialName("session_id") val sessionId: String,
        @SerialName("last_seen_seq") val lastSeenSeq: Long,
        val ticket: String?,
        val client: ClientInfo,
        @SerialName("requested_capabilities")
        val requestedCapabilities: List<ClientCapability>,
    ) : ClientMessage

    @Serializable
    @SerialName("prompt")
    data class Prompt(
        @SerialName("request_id") val requestId: String,
        val text: String,
    ) : ClientMessage

    @Serializable
    @SerialName("approval_response")
    data class ApprovalResponse(
        @SerialName("approval_id") val approvalId: String,
        val decision: ApprovalDecision,
    ) : ClientMessage

    @Serializable
    @SerialName("interrupt")
    data class Interrupt(
        @SerialName("request_id") val requestId: String? = null,
    ) : ClientMessage

    @Serializable
    @SerialName("stop_session")
    data class StopSession(
        @SerialName("request_id") val requestId: String,
    ) : ClientMessage

    @Serializable
    @SerialName("clear_history")
    data class ClearHistory(
        @SerialName("request_id") val requestId: String,
    ) : ClientMessage

    @Serializable
    @SerialName("get_usage")
    data class GetUsage(
        @SerialName("request_id") val requestId: String,
    ) : ClientMessage

    @Serializable
    @SerialName("list_sessions")
    data class ListSessions(
        @SerialName("request_id") val requestId: String,
        val cursor: String? = null,
    ) : ClientMessage

    @Serializable
    @SerialName("sync_request")
    data class SyncRequest(
        @SerialName("last_seen_seq") val lastSeenSeq: Long,
    ) : ClientMessage

    @Serializable
    @SerialName("set_config")
    data class SetConfig(
        @SerialName("request_id") val requestId: String? = null,
        @SerialName("config_id") val configId: String,
        val value: RuntimeValue,
    ) : ClientMessage

    @Serializable
    @SerialName("ping")
    data object Ping : ClientMessage

    @Serializable
    @SerialName("detach")
    data object Detach : ClientMessage
}

@Serializable
@JsonClassDiscriminator("type")
sealed interface ServerMessage {
    @Serializable
    @SerialName("attach_ok")
    data class AttachOk(
        @SerialName("protocol_version") val protocolVersion: Int,
        @SerialName("session_id") val sessionId: String,
        @SerialName("granted_capabilities")
        val grantedCapabilities: List<ClientCapability>,
        val seq: Long,
    ) : ServerMessage

    @Serializable
    @SerialName("attach_denied")
    data class AttachDenied(val reason: String) : ServerMessage

    @Serializable
    @SerialName("event")
    data class Event(
        val seq: Long,
        val event: SessionEvent,
    ) : ServerMessage

    @Serializable
    @SerialName("snapshot")
    data class Snapshot(
        val seq: Long,
        val state: SessionSnapshot,
    ) : ServerMessage

    @Serializable
    @SerialName("error")
    data class Error(
        @SerialName("request_id") val requestId: String? = null,
        val code: String,
        val message: String,
    ) : ServerMessage

    @Serializable
    @SerialName("command_result")
    data class CommandResult(
        @SerialName("request_id") val requestId: String,
        val operation: String,
        val changed: Boolean,
    ) : ServerMessage

    @Serializable
    @SerialName("session_list")
    data class SessionList(
        @SerialName("request_id") val requestId: String,
        val sessions: List<SessionListEntry>,
        @SerialName("next_cursor") val nextCursor: String? = null,
    ) : ServerMessage

    @Serializable
    @SerialName("usage")
    data class Usage(
        @SerialName("request_id") val requestId: String,
        val usage: SessionUsage,
    ) : ServerMessage

    @Serializable
    @SerialName("pong")
    data object Pong : ServerMessage

    @Serializable
    @SerialName("revoked")
    data class Revoked(val reason: String) : ServerMessage
}

@Serializable
enum class TurnStatus {
    @SerialName("idle") IDLE,
    @SerialName("running") RUNNING,
    @SerialName("waiting_for_approval") WAITING_FOR_APPROVAL,
    @SerialName("cancelling") CANCELLING,
    @SerialName("cancelled") CANCELLED,
}

@Serializable
enum class TranscriptRole {
    @SerialName("user") USER,
    @SerialName("assistant") ASSISTANT,
    @SerialName("system") SYSTEM,
    @SerialName("thought") THOUGHT,
}

@Serializable
data class TranscriptEntry(
    val id: Long,
    val role: TranscriptRole,
    val text: String,
    val outcome: AssistantOutcome? = null,
)

@Serializable
data class SessionListEntry(
    @SerialName("session_id") val sessionId: String,
    val active: Boolean,
    @SerialName("attached_clients") val attachedClients: Int,
)

@Serializable
data class SessionUsage(
    val input: Long,
    val output: Long,
    @SerialName("reasoning_output") val reasoningOutput: Long? = null,
    @SerialName("thinking_bytes") val thinkingBytes: Long,
    @SerialName("cache_read") val cacheRead: Long,
    @SerialName("cache_write") val cacheWrite: Long,
    @SerialName("cost_usd_micros") val costUsdMicros: Long,
)

@Serializable
enum class ToolCallStatus {
    @SerialName("pending") PENDING,
    @SerialName("in_progress") IN_PROGRESS,
    @SerialName("completed") COMPLETED,
    @SerialName("failed") FAILED,
    @SerialName("cancelled") CANCELLED,
}

@Serializable
data class ToolCallState(
    val id: String,
    val name: String,
    val title: String,
    val status: ToolCallStatus,
    val output: String? = null,
)

@Serializable
data class ApprovalRequest(
    val id: String,
    @SerialName("tool_call_id") val toolCallId: String,
    val tool: String,
    val detail: String,
    @SerialName("allow_always_available") val allowAlwaysAvailable: Boolean,
)

@Serializable
@JsonClassDiscriminator("type")
sealed interface AssistantOutcome {
    @Serializable
    @SerialName("completed")
    data object Completed : AssistantOutcome

    @Serializable
    @SerialName("errored")
    data class Errored(
        @SerialName("context_overflow") val contextOverflow: Boolean,
        val message: String,
    ) : AssistantOutcome

    @Serializable
    @SerialName("refused")
    data object Refused : AssistantOutcome

    @Serializable
    @SerialName("aborted")
    data object Aborted : AssistantOutcome

    @Serializable
    @SerialName("max_tokens")
    data object MaxTokens : AssistantOutcome
}

@Serializable
@JsonClassDiscriminator("type")
sealed interface SessionEvent {
    @Serializable
    @SerialName("user_message")
    data class UserMessage(
        val text: String,
        @SerialName("request_id") val requestId: String? = null,
    ) : SessionEvent

    @Serializable
    @SerialName("assistant_delta")
    data class AssistantDelta(val text: String) : SessionEvent

    @Serializable
    @SerialName("assistant_done")
    data class AssistantDone(val outcome: AssistantOutcome) : SessionEvent

    @Serializable
    @SerialName("thought_delta")
    data class ThoughtDelta(val text: String) : SessionEvent

    @Serializable
    @SerialName("tool_call_started")
    data class ToolCallStarted(
        val id: String,
        val name: String,
        val title: String,
        @SerialName("input_summary") val inputSummary: String? = null,
    ) : SessionEvent

    @Serializable
    @SerialName("tool_call_updated")
    data class ToolCallUpdated(
        val id: String,
        val status: ToolCallStatus,
    ) : SessionEvent

    @Serializable
    @SerialName("tool_call_finished")
    data class ToolCallFinished(
        val id: String,
        val status: ToolCallStatus,
        val output: String,
    ) : SessionEvent

    @Serializable
    @SerialName("approval_requested")
    data class ApprovalRequested(val request: ApprovalRequest) : SessionEvent

    @Serializable
    @SerialName("approval_resolved")
    data class ApprovalResolved(
        @SerialName("approval_id") val approvalId: String,
        val decision: ApprovalDecision,
        @SerialName("resolved_by") val resolvedBy: String,
    ) : SessionEvent

    @Serializable
    @SerialName("runtime_options_changed")
    data class RuntimeOptionsChanged(val options: List<RuntimeOption>) : SessionEvent

    @Serializable
    @SerialName("context_usage_changed")
    data class ContextUsageChanged(val usage: ContextUsage) : SessionEvent

    @Serializable
    @SerialName("conversation_cleared")
    data object ConversationCleared : SessionEvent

    @Serializable
    @SerialName("turn_status_changed")
    data class TurnStatusChanged(val status: TurnStatus) : SessionEvent

    @Serializable
    @SerialName("session_ending")
    data class SessionEnding(val reason: String) : SessionEvent
}

@Serializable
data class SessionSnapshot(
    @SerialName("session_id") val sessionId: String,
    val seq: Long,
    @SerialName("turn_status") val turnStatus: TurnStatus,
    val transcript: List<TranscriptEntry>,
    @SerialName("tool_calls") val toolCalls: List<ToolCallState>,
    @SerialName("pending_approvals") val pendingApprovals: List<ApprovalRequest>,
    @SerialName("runtime_options") val runtimeOptions: List<RuntimeOption>,
    @SerialName("context_usage") val contextUsage: ContextUsage? = null,
    @SerialName("history_truncated") val historyTruncated: Boolean = false,
)

@Serializable
data class RuntimeChoice(
    val id: String,
    val label: String,
)

@Serializable
@JsonClassDiscriminator("type")
sealed interface RuntimeOptionSpec {
    @Serializable
    @SerialName("select")
    data class Select(val choices: List<RuntimeChoice>) : RuntimeOptionSpec

    @Serializable
    @SerialName("boolean")
    data object BooleanValue : RuntimeOptionSpec

    @Serializable
    @SerialName("integer")
    data class IntegerValue(
        val min: Long,
        val max: Long,
        val step: Long,
    ) : RuntimeOptionSpec
}

@Serializable
data class RuntimeOption(
    val id: String,
    val label: String,
    val value: RuntimeValue,
    @SerialName("default_value") val defaultValue: RuntimeValue,
    val spec: RuntimeOptionSpec,
    @SerialName("mutable_while_running") val mutableWhileRunning: Boolean,
    val help: String? = null,
)

@Serializable(with = RuntimeValueSerializer::class)
sealed interface RuntimeValue {
    data class StringValue(val value: String) : RuntimeValue
    data class BooleanValue(val value: Boolean) : RuntimeValue
    data class IntegerValue(val value: Long) : RuntimeValue
}

object RuntimeValueSerializer : KSerializer<RuntimeValue> {
    override val descriptor: SerialDescriptor =
        PrimitiveSerialDescriptor("RuntimeValue", PrimitiveKind.STRING)

    override fun serialize(encoder: Encoder, value: RuntimeValue) {
        val jsonEncoder = encoder as? JsonEncoder
            ?: throw SerializationException("RuntimeValue requires JSON")
        val primitive = when (value) {
            is RuntimeValue.StringValue -> JsonPrimitive(value.value)
            is RuntimeValue.BooleanValue -> JsonPrimitive(value.value)
            is RuntimeValue.IntegerValue -> JsonPrimitive(value.value)
        }
        jsonEncoder.encodeJsonElement(primitive)
    }

    override fun deserialize(decoder: Decoder): RuntimeValue {
        val jsonDecoder = decoder as? JsonDecoder
            ?: throw SerializationException("RuntimeValue requires JSON")
        val primitive = jsonDecoder.decodeJsonElement() as? JsonPrimitive
            ?: throw SerializationException("RuntimeValue must be a primitive")
        if (primitive.isString) {
            return RuntimeValue.StringValue(primitive.content)
        }
        primitive.booleanOrNull?.let { return RuntimeValue.BooleanValue(it) }
        primitive.longOrNull?.let { return RuntimeValue.IntegerValue(it) }
        throw SerializationException("RuntimeValue must be a string, boolean, or integer")
    }
}

@Serializable
enum class ContextBudgetError {
    @SerialName("output_reservation_not_below_window")
    OUTPUT_RESERVATION_NOT_BELOW_WINDOW,
}

@Serializable
data class ContextUsage(
    @SerialName("prompt_tokens") val promptTokens: Long,
    val estimated: Boolean = false,
    @SerialName("model_context_window") val modelContextWindow: Long? = null,
    @SerialName("output_reservation") val outputReservation: Long,
    @SerialName("effective_input_budget") val effectiveInputBudget: Long? = null,
    @SerialName("utilization_basis_points") val utilizationBasisPoints: Int? = null,
    @SerialName("compaction_high_water_tokens") val compactionHighWaterTokens: Long? = null,
    @SerialName("budget_error") val budgetError: ContextBudgetError? = null,
)
