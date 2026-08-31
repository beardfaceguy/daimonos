package dev.daimonos.remote.ui

import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import dev.daimonos.remote.protocol.ApprovalDecision
import dev.daimonos.remote.protocol.ClientCapability
import dev.daimonos.remote.protocol.DurabilityStatus
import dev.daimonos.remote.protocol.TimelineEntry
import dev.daimonos.remote.protocol.TurnStatus
import dev.daimonos.remote.session.displayText

@Composable
@OptIn(ExperimentalMaterial3Api::class)
fun DaimonosRemoteApp(viewModel: ControllerViewModel = viewModel()) {
    val state by viewModel.state.collectAsStateWithLifecycle()
    MaterialTheme {
        Scaffold(
            topBar = {
                TopAppBar(
                    title = { Text("Daimonos Remote") },
                    actions = {
                        if (state.mode == AppMode.SESSION || state.mode == AppMode.CONNECTING) {
                            TextButton(onClick = viewModel::forgetDevice) {
                                Text("Forget")
                            }
                        }
                    },
                )
            },
        ) { contentPadding ->
            when (state.mode) {
                AppMode.LOADING -> LoadingScreen(
                    "Loading secure credentials…",
                    Modifier.padding(contentPadding),
                )
                AppMode.PAIRING -> PairingScreen(
                    error = state.error,
                    onPair = viewModel::pair,
                    modifier = Modifier.padding(contentPadding),
                )
                AppMode.WAITING_FOR_LOCAL_APPROVAL -> LoadingScreen(
                    state.pairingFingerprint?.let {
                        "Approve fingerprint $it on the Daimonos host"
                    } ?: "Waiting for the host to show this device fingerprint…",
                    Modifier.padding(contentPadding),
                )
                AppMode.CONNECTING -> LoadingScreen(
                    "Connecting to the Daimonos host…",
                    Modifier.padding(contentPadding),
                )
                AppMode.SESSION -> SessionScreen(
                    state = state,
                    viewModel = viewModel,
                    modifier = Modifier.padding(contentPadding),
                )
            }
        }
    }
}

@Composable
private fun LoadingScreen(message: String, modifier: Modifier = Modifier) {
    Column(
        modifier = modifier
            .fillMaxSize()
            .padding(32.dp),
        verticalArrangement = Arrangement.spacedBy(20.dp),
    ) {
        CircularProgressIndicator()
        Text(message)
    }
}

@Composable
private fun PairingScreen(
    error: String?,
    onPair: (String, String, String) -> Unit,
    modifier: Modifier = Modifier,
) {
    var endpoint by rememberSaveable { mutableStateOf("") }
    var claim by rememberSaveable { mutableStateOf("") }
    var label by rememberSaveable { mutableStateOf(android.os.Build.MODEL.take(120)) }
    Column(
        modifier = modifier
            .fillMaxSize()
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("Pair this phone", style = MaterialTheme.typography.headlineSmall)
        Text("Enter the WSS endpoint and single-use claim printed by the host.")
        OutlinedTextField(
            value = endpoint,
            onValueChange = { endpoint = it },
            label = { Text("wss:// endpoint") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = claim,
            onValueChange = { claim = it },
            label = { Text("Pairing claim") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = label,
            onValueChange = { label = it.take(120) },
            label = { Text("Device label") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        Button(
            onClick = { onPair(endpoint, claim, label) },
            enabled = endpoint.isNotBlank() && claim.isNotBlank() && label.isNotBlank(),
        ) {
            Text("Request pairing")
        }
    }
}

@Composable
private fun SessionScreen(
    state: ControllerUiState,
    viewModel: ControllerViewModel,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier
            .fillMaxSize()
            .padding(horizontal = 12.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .horizontalScroll(rememberScrollState()),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedButton(onClick = { viewModel.switchSession(null) }) {
                Text("New")
            }
            state.sessions.forEach { session ->
                OutlinedButton(onClick = { viewModel.switchSession(session.sessionId) }) {
                    Text(session.sessionId.take(12))
                }
            }
            OutlinedButton(
                onClick = viewModel::stopSession,
                enabled = ClientCapability.STOP in state.grantedCapabilities,
            ) {
                Text("Stop")
            }
        }
        Text(
            buildString {
                append(if (state.connected) "Connected" else "Reconnecting")
                append(" · ")
                append(state.session.turnStatus.name.lowercase())
                state.session.durabilityStatus.statusLabel()?.let { durability ->
                    append(" · ")
                    append(durability)
                }
                state.session.contextUsage?.utilizationBasisPoints?.let {
                    append(" · context ")
                    append(it / 100.0)
                    append('%')
                }
            },
            style = MaterialTheme.typography.labelLarge,
        )
        state.error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        if (state.session.historyWindow.truncatedBefore > 0) {
            Text(
                "Earlier history was truncated.",
                color = MaterialTheme.colorScheme.tertiary,
                style = MaterialTheme.typography.labelMedium,
            )
        }
        LazyColumn(
            modifier = Modifier.weight(1f),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(state.session.timeline, key = TimelineEntry::id) { entry ->
                Card(Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(12.dp)) {
                        when (entry) {
                            is TimelineEntry.User -> {
                                Text("user", style = MaterialTheme.typography.labelSmall)
                                Text(entry.text)
                            }
                            is TimelineEntry.Assistant -> {
                                Text("assistant", style = MaterialTheme.typography.labelSmall)
                                Text(entry.text)
                            }
                            is TimelineEntry.Thought -> {
                                Text("thought", style = MaterialTheme.typography.labelSmall)
                                Text(entry.text)
                            }
                            is TimelineEntry.System -> {
                                Text("system", style = MaterialTheme.typography.labelSmall)
                                Text(entry.text)
                            }
                            is TimelineEntry.Outcome ->
                                Text(entry.outcome.displayText(), style = MaterialTheme.typography.labelSmall)
                            is TimelineEntry.Tool -> {
                                Text("${entry.title} · ${entry.status.name.lowercase()}")
                                entry.output?.let { Text(it) }
                            }
                        }
                    }
                }
            }
            items(
                state.session.activeTools.filterNot { active ->
                    state.session.timeline.any { it.id == active.occurrenceId }
                },
                key = { "active-tool:${it.occurrenceId}" },
            ) { tool ->
                Card(Modifier.fillMaxWidth()) {
                    Column(Modifier.padding(12.dp)) {
                        Text("${tool.title} · ${tool.status.name.lowercase()}")
                    }
                }
            }
            items(state.session.pendingApprovals, key = { "approval:${it.id}" }) { approval ->
                Card(Modifier.fillMaxWidth()) {
                    Column(
                        Modifier.padding(12.dp),
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        Text("Approval: ${approval.tool}")
                        Text(approval.detail)
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            Button(
                                onClick = {
                                    viewModel.answerApproval(
                                        approval.id,
                                        ApprovalDecision.ALLOW_ONCE,
                                    )
                                },
                                enabled = ClientCapability.APPROVE_ONCE in state.grantedCapabilities,
                            ) { Text("Allow once") }
                            OutlinedButton(
                                onClick = {
                                    viewModel.answerApproval(approval.id, ApprovalDecision.DENY)
                                },
                                enabled = ClientCapability.APPROVE_ONCE in state.grantedCapabilities,
                            ) { Text("Deny") }
                            if (
                                approval.allowAlwaysAvailable &&
                                ClientCapability.APPROVE_ALWAYS in state.grantedCapabilities
                            ) {
                                OutlinedButton(onClick = {
                                    viewModel.answerApproval(
                                        approval.id,
                                        ApprovalDecision.ALLOW_ALWAYS,
                                    )
                                }) { Text("Always") }
                            }
                        }
                    }
                }
            }
        }
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = state.promptDraft,
                onValueChange = viewModel::updatePromptDraft,
                label = { Text("Prompt") },
                modifier = Modifier.weight(1f),
            )
            Button(
                onClick = {
                    viewModel.sendPrompt(state.promptDraft)
                },
                enabled = state.connected &&
                    state.promptDraft.isNotBlank() &&
                    !state.promptPending &&
                    state.session.turnStatus == TurnStatus.IDLE &&
                    ClientCapability.PROMPT in state.grantedCapabilities,
            ) {
                Text("Send")
            }
            OutlinedButton(
                onClick = viewModel::interrupt,
                enabled = state.connected &&
                    ClientCapability.INTERRUPT in state.grantedCapabilities,
            ) {
                Text("Interrupt")
            }
        }
    }
}

private fun DurabilityStatus.statusLabel(): String? = when (this) {
    DurabilityStatus.SAVED -> null
    DurabilityStatus.UNSAVED -> "unsaved"
    DurabilityStatus.SAVING -> "saving"
    DurabilityStatus.DEGRADED -> "save degraded"
    DurabilityStatus.SUPERSEDED -> "persistence superseded"
}
