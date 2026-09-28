package com.rastislavkish.rconnect

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.view.KeyEvent
import android.view.WindowManager
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.lifecycle.lifecycleScope
import kotlin.math.roundToInt
import com.rastislavkish.rconnect.ui.theme.RConnectTheme
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.rconnect_ffi.ConnectionPath
import uniffi.rconnect_ffi.RConnectException
import uniffi.rconnect_ffi.SessionHandle
import uniffi.rconnect_ffi.SessionListener

/**
 * A running session: keys go to the computer, its screen reader speaks through the
 * phone. It ends with CapsLock+F11, the End button, or by leaving this screen.
 */
class SessionActivity : ComponentActivity() {

    private var status by mutableStateOf("")
    /** Last text spoken by the computer, shown for sighted helpers; never announced. */
    private var lastSpeech by mutableStateOf("")
    private var session: SessionHandle? = null
    private var ended = false
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        val hostId = intent.getStringExtra(EXTRA_HOST_ID) ?: return finish()
        val hostName = intent.getStringExtra(EXTRA_HOST_NAME).orEmpty()
        status = getString(R.string.connecting_to, hostName)
        acquireWifiLock()

        setContent {
            RConnectTheme {
                Scaffold { padding -> SessionScreen(Modifier.padding(padding)) }
            }
        }
        start(hostId, hostName)
    }

    @Composable
    private fun SessionScreen(modifier: Modifier) {
        Column(
            modifier.fillMaxSize().padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                status,
                style = MaterialTheme.typography.headlineSmall,
                modifier = Modifier.semantics { heading() },
            )
            Button(onClick = { end() }, modifier = Modifier.fillMaxWidth()) {
                Text(getString(R.string.end_session))
            }
            Button(onClick = { session?.ping() }, modifier = Modifier.fillMaxWidth()) {
                Text(getString(R.string.ping))
            }
            Text(lastSpeech)
        }
    }

    private fun start(hostId: String, hostName: String) {
        lifecycleScope.launch {
            try {
                val core = app.core.await()
                val speechData = app.speechData.await()
                val voice = withContext(Dispatchers.IO) { app.settings.voiceSettings(speechData) }
                val handle = core.startSession(
                    hostId,
                    speechData.path,
                    voice,
                    app.settings.keepAlive,
                    Listener(),
                )
                if (ended) {
                    handle.end()
                    return@launch
                }
                session = handle
                KeyRouter.attach(handle) { end() }
                status = getString(R.string.connected_to, hostName)
            } catch (e: RConnectException) {
                status = e.message ?: getString(R.string.connection_failed)
                window.decorView.announceForAccessibility(status)
            }
        }
    }

    private inner class Listener : SessionListener {
        override fun onSpeech(text: String) {
            runOnUiThread { lastSpeech = text }
        }

        override fun onPing(milliseconds: Double?, path: ConnectionPath) {
            val pathName = getString(
                when (path) {
                    ConnectionPath.DIRECT -> R.string.path_direct
                    ConnectionPath.RELAYED -> R.string.path_relayed
                    ConnectionPath.UNKNOWN -> R.string.path_unknown
                },
            )
            val message = if (milliseconds == null) {
                getString(R.string.ping_no_answer, pathName)
            } else {
                getString(R.string.ping_result, milliseconds.roundToInt(), pathName)
            }
            runOnUiThread { Toast.makeText(this@SessionActivity, message, Toast.LENGTH_SHORT).show() }
        }

        override fun onEnded(reason: String) {
            runOnUiThread {
                session?.let { KeyRouter.detach(it) }
                session = null
                status = reason
                if (!ended) {
                    ended = true
                    window.decorView.announceForAccessibility(reason)
                }
                finish()
            }
        }
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean =
        KeyRouter.handle(event) || super.dispatchKeyEvent(event)

    /** Leaving the screen ends the session. */
    override fun onStop() {
        super.onStop()
        if (!isChangingConfigurations) end()
    }

    override fun onDestroy() {
        super.onDestroy()
        wifiLock?.release()
        wifiLock = null
    }

    /**
     * Keeps Wi-Fi out of power saving during the session. Otherwise the phone's radio
     * sleeps between the router's beacons, and speech from the computer waits at the
     * router for up to about 100 ms.
     */
    @Suppress("DEPRECATION")
    private fun acquireWifiLock() {
        val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            WifiManager.WIFI_MODE_FULL_LOW_LATENCY
        } else {
            WifiManager.WIFI_MODE_FULL_HIGH_PERF
        }
        wifiLock = wifi.createWifiLock(mode, "RConnect session").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun end() {
        if (ended) return
        ended = true
        session?.let {
            KeyRouter.detach(it)
            it.end()
        }
        finish()
    }

    companion object {
        const val EXTRA_HOST_ID = "host_id"
        const val EXTRA_HOST_NAME = "host_name"
    }
}
