package dev.daimonos.remote.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

@Composable
@OptIn(ExperimentalMaterial3Api::class)
fun DaimonosRemoteApp() {
    MaterialTheme {
        Scaffold(
            topBar = {
                TopAppBar(title = { Text("Daimonos Remote") })
            },
        ) { contentPadding ->
            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(contentPadding)
                    .padding(24.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Text(
                    text = "No daemon paired",
                    style = MaterialTheme.typography.headlineSmall,
                )
                Text(
                    text = "Pairing and session controls arrive in the next slice.",
                    style = MaterialTheme.typography.bodyLarge,
                )
            }
        }
    }
}
