package com.rastislavkish.rconnect

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.lifecycle.lifecycleScope
import com.rastislavkish.rconnect.ui.theme.RConnectTheme
import kotlinx.coroutines.launch
import uniffi.rconnect_ffi.RConnectException

/**
 * Pairs with a computer from the link `rconnect pair` shows. The link can be pasted, or
 * opened from any QR code scanner app, which starts this screen with it.
 */
class PairActivity : ComponentActivity() {

    private var link by mutableStateOf("")
    private var status by mutableStateOf("")
    private var pairing by mutableStateOf(false)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        intent?.data?.toString()?.let { uri ->
            link = uri
            pair()
        }
        setContent {
            RConnectTheme {
                Scaffold { padding -> PairScreen(Modifier.padding(padding)) }
            }
        }
    }

    @Composable
    private fun PairScreen(modifier: Modifier) {
        Column(
            modifier.fillMaxSize().padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                getString(R.string.pair_computer),
                style = MaterialTheme.typography.headlineSmall,
                modifier = Modifier.semantics { heading() },
            )
            Text(getString(R.string.pair_instructions))
            OutlinedTextField(
                value = link,
                onValueChange = { link = it },
                label = { Text(getString(R.string.pairing_link)) },
                enabled = !pairing,
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = { pair() },
                enabled = !pairing && link.isNotBlank(),
                modifier = Modifier.fillMaxWidth(),
            ) { Text(getString(R.string.pair)) }
            if (status.isNotEmpty()) {
                Text(
                    status,
                    Modifier.semantics { liveRegion = LiveRegionMode.Polite },
                )
            }
        }
    }

    private fun pair() {
        val uri = link.trim()
        pairing = true
        lifecycleScope.launch {
            try {
                val core = app.core.await()
                val words = core.readTicket(uri).words.joinToString(", ")
                status = getString(R.string.pairing_words, words)
                val host = core.pair(uri)
                status = getString(R.string.paired_with, host.name)
                window.decorView.announceForAccessibility(status)
                finish()
            } catch (e: RConnectException) {
                status = e.message ?: getString(R.string.pairing_failed)
            } finally {
                pairing = false
            }
        }
    }
}
