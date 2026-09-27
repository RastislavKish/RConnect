package com.rastislavkish.rconnect

import android.content.Intent
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.selection.toggleable
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.lifecycle.lifecycleScope
import com.rastislavkish.rconnect.ui.theme.RConnectTheme
import kotlinx.coroutines.launch
import uniffi.rconnect_ffi.HostInfo

class MainActivity : ComponentActivity() {

    private var hosts by mutableStateOf<List<HostInfo>>(emptyList())
    private var fingerprint by mutableStateOf("")
    private var error by mutableStateOf<String?>(null)
    private var serviceEnabled by mutableStateOf(false)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            RConnectTheme {
                Scaffold { padding ->
                    MainScreen(Modifier.padding(padding))
                }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        serviceEnabled = KeyCaptureService.isEnabled(this)
        lifecycleScope.launch {
            try {
                val core = app.core.await()
                hosts = core.hosts()
                fingerprint = core.fingerprint().joinToString(" ")
                error = null
            } catch (e: Exception) {
                error = getString(R.string.core_failed, e.message)
            }
        }
    }

    @Composable
    private fun MainScreen(modifier: Modifier) {
        var keepAlive by remember { mutableStateOf(app.settings.keepAlive) }
        var hostToRemove by remember { mutableStateOf<HostInfo?>(null) }

        Column(
            modifier.fillMaxSize().padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                getString(R.string.computers),
                style = MaterialTheme.typography.headlineSmall,
                modifier = Modifier.semantics { heading() },
            )
            error?.let { Text(it) }
            if (hosts.isEmpty() && error == null) Text(getString(R.string.no_computers))
            LazyColumn(
                Modifier.weight(1f, fill = false),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                items(hosts, key = { it.id }) { host ->
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Button(
                            onClick = { connect(host) },
                            modifier = Modifier.weight(1f),
                        ) { Text(getString(R.string.connect_to, host.name)) }
                        TextButton(onClick = { hostToRemove = host }) {
                            Text(getString(R.string.remove_named, host.name))
                        }
                    }
                }
            }
            OutlinedButton(
                onClick = { startActivity(Intent(this@MainActivity, PairActivity::class.java)) },
                modifier = Modifier.fillMaxWidth(),
            ) { Text(getString(R.string.pair_computer)) }

            Row(
                Modifier
                    .fillMaxWidth()
                    .toggleable(value = keepAlive, role = Role.Switch) {
                        keepAlive = it
                        app.settings.keepAlive = it
                    },
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(getString(R.string.keep_alive), Modifier.weight(1f))
                Switch(checked = keepAlive, onCheckedChange = null)
            }

            if (!serviceEnabled) {
                Text(getString(R.string.service_disabled))
                OutlinedButton(
                    onClick = { startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)) },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text(getString(R.string.open_accessibility_settings)) }
            }
            if (fingerprint.isNotEmpty()) {
                Text(getString(R.string.phone_fingerprint, fingerprint))
            }
        }

        hostToRemove?.let { host ->
            AlertDialog(
                onDismissRequest = { hostToRemove = null },
                title = { Text(getString(R.string.remove_question, host.name)) },
                text = { Text(getString(R.string.remove_explanation)) },
                confirmButton = {
                    TextButton(onClick = {
                        hostToRemove = null
                        remove(host)
                    }) { Text(getString(R.string.remove)) }
                },
                dismissButton = {
                    TextButton(onClick = { hostToRemove = null }) {
                        Text(getString(R.string.cancel))
                    }
                },
            )
        }
    }

    private fun connect(host: HostInfo) {
        startActivity(
            Intent(this, SessionActivity::class.java)
                .putExtra(SessionActivity.EXTRA_HOST_ID, host.id)
                .putExtra(SessionActivity.EXTRA_HOST_NAME, host.name),
        )
    }

    private fun remove(host: HostInfo) {
        lifecycleScope.launch {
            val core = app.core.await()
            core.removeHost(host.id)
            hosts = core.hosts()
        }
    }
}
