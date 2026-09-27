package com.rastislavkish.rconnect

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
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
import uniffi.rconnect_ffi.DiscoveredHost
import uniffi.rconnect_ffi.DiscoveryHandle
import uniffi.rconnect_ffi.DiscoveryListener
import uniffi.rconnect_ffi.HostInfo
import uniffi.rconnect_ffi.RConnectException

/**
 * Pairs with a computer found on the local network, or from the link `rconnect pair`
 * shows. The link can be pasted, or opened from any QR code scanner app, which starts
 * this screen with it.
 */
class PairActivity : ComponentActivity() {

    private var link by mutableStateOf("")
    private var status by mutableStateOf("")
    private var pairing by mutableStateOf(false)
    private val found = mutableStateListOf<DiscoveredHost>()

    private var discovery: DiscoveryHandle? = null
    private var multicastLock: WifiManager.MulticastLock? = null
    private var resumed = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        intent?.data?.toString()?.let { uri ->
            link = uri
            pairWithLink()
        }
        setContent {
            RConnectTheme {
                Scaffold { padding -> PairScreen(Modifier.padding(padding)) }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        resumed = true
        startDiscovery()
    }

    override fun onPause() {
        super.onPause()
        resumed = false
        stopDiscovery()
    }

    /** mDNS needs a multicast lock on Android, held only while this screen is shown. */
    private fun startDiscovery() {
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("RConnect discovery").apply {
            setReferenceCounted(false)
            acquire()
        }
        lifecycleScope.launch {
            val handle = app.core.await().startDiscovery(Listener())
            if (resumed && discovery == null) discovery = handle else handle.stop()
        }
    }

    private fun stopDiscovery() {
        discovery?.stop()
        discovery = null
        multicastLock?.release()
        multicastLock = null
        found.clear()
    }

    private inner class Listener : DiscoveryListener {
        override fun onFound(host: DiscoveredHost) {
            runOnUiThread {
                found.removeAll { it.id == host.id }
                found.add(host)
                found.sortBy { it.name.lowercase() }
            }
        }

        override fun onLost(id: String) {
            runOnUiThread { found.removeAll { it.id == id } }
        }
    }

    @Composable
    private fun PairScreen(modifier: Modifier) {
        Column(
            modifier
                .fillMaxSize()
                .padding(16.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Heading(getString(R.string.pair_computer))
            Text(getString(R.string.pair_instructions))
            if (status.isNotEmpty()) {
                Text(status, Modifier.semantics { liveRegion = LiveRegionMode.Polite })
            }

            Heading(getString(R.string.local_computers))
            if (found.isEmpty()) Text(getString(R.string.searching))
            for (host in found) {
                OutlinedButton(
                    onClick = { pairWithDiscovered(host) },
                    enabled = !pairing,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(
                        if (host.paired) getString(R.string.pair_again_with, host.name)
                        else getString(R.string.pair_with, host.name),
                    )
                }
            }

            Heading(getString(R.string.pairing_link))
            OutlinedTextField(
                value = link,
                onValueChange = { link = it },
                label = { Text(getString(R.string.pairing_link)) },
                enabled = !pairing,
                modifier = Modifier.fillMaxWidth(),
            )
            Button(
                onClick = { pairWithLink() },
                enabled = !pairing && link.isNotBlank(),
                modifier = Modifier.fillMaxWidth(),
            ) { Text(getString(R.string.pair)) }
        }
    }

    @Composable
    private fun Heading(text: String) {
        Text(
            text,
            style = MaterialTheme.typography.titleLarge,
            modifier = Modifier.semantics { heading() },
        )
    }

    private fun pairWithLink() {
        val uri = link.trim()
        pair {
            val core = app.core.await()
            val words = core.readTicket(uri).words.joinToString(", ")
            status = getString(R.string.pairing_words, words)
            core.pair(uri)
        }
    }

    private fun pairWithDiscovered(host: DiscoveredHost) {
        pair {
            status = getString(R.string.pairing_words, host.words.joinToString(", "))
            app.core.await().pairDiscovered(host.id)
        }
    }

    private fun pair(action: suspend () -> HostInfo) {
        pairing = true
        lifecycleScope.launch {
            try {
                val host = action()
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
