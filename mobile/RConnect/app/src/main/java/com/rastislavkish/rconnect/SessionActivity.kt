package com.rastislavkish.rconnect

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.media.AudioRecordingConfiguration
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.view.KeyEvent
import android.view.WindowManager
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
import uniffi.rconnect_ffi.AudioStreamMode
import uniffi.rconnect_ffi.ConnectionPath
import uniffi.rconnect_ffi.MicSettings
import uniffi.rconnect_ffi.MicStreamMode
import uniffi.rconnect_ffi.RConnectException
import uniffi.rconnect_ffi.SessionHandle
import uniffi.rconnect_ffi.SessionListener

/**
 * A running session: keys go to the computer, its screen reader speaks through the
 * phone. It ends with CapsLock+F11, the End button, or by leaving this screen.
 * CapsLock+F10 switches the computer's audio stream through the modes chosen in the
 * settings, in their order, and CapsLock+F9 the phone's microphone on the computer.
 */
class SessionActivity : ComponentActivity(), KeyRouter.Shortcuts {

    private var status by mutableStateOf("")
    /** Last text spoken by the computer, shown for sighted helpers; never announced. */
    private var lastSpeech by mutableStateOf("")
    /** The last ping result, kept on screen after it is announced. */
    private var lastPing by mutableStateOf("")
    private var session: SessionHandle? = null
    private var ended = false
    private var wifiLock: WifiManager.WifiLock? = null
    /** The audio mode last asked for; the computer confirms it through `onAudio`. */
    private var audioMode = AudioStreamMode.OFF
    /** The modes CapsLock+F10 cycles through, in the user's order. */
    private var audioCycle = listOf(AudioStreamMode.OFF)
    /** The microphone mode last asked for, and the modes CapsLock+F9 cycles through. */
    private var micMode = MicStreamMode.OFF
    private var micCycle = listOf(MicStreamMode.OFF)
    /** Once the computer has the microphone, Off is skipped unless the user allowed it. */
    private var micCreated = false
    private var micAllowRemove = false
    private var micPermitted = false
    private var recordingCallback: AudioManager.AudioRecordingCallback? = null

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
            if (lastPing.isNotEmpty()) Text(lastPing)
            Text(lastSpeech)
        }
    }

    private fun start(hostId: String, hostName: String) {
        lifecycleScope.launch {
            try {
                val core = app.core.await()
                val speechData = app.speechData.await()
                val voice = withContext(Dispatchers.IO) { app.settings.voiceSettings(speechData) }
                val audio = app.settings.audioSettings()
                audioCycle = app.settings.audioCycle()
                audioMode = audio.mode
                micCycle = app.settings.micCycle()
                micMode = micCycle.first()
                micCreated = micMode != MicStreamMode.OFF
                micAllowRemove = app.settings.micAllowRemove
                micPermitted = checkSelfPermission(Manifest.permission.RECORD_AUDIO) ==
                    PackageManager.PERMISSION_GRANTED
                val handle = core.startSession(
                    hostId,
                    speechData.path,
                    voice,
                    app.settings.keepAlive,
                    audio,
                    MicSettings(micMode, micPermitted, builtInMicrophone()),
                    Listener(),
                )
                watchMicrophoneSilencing()
                if (ended) {
                    handle.end()
                    return@launch
                }
                session = handle
                KeyRouter.attach(handle, this@SessionActivity)
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
            // Announced rather than shown as a toast, which TalkBack does not always read.
            runOnUiThread {
                lastPing = message
                window.decorView.announceForAccessibility(message)
            }
        }

        override fun onAudio(mode: AudioStreamMode, available: Boolean) {
            val message = if (available) {
                getString(R.string.audio_mode_now, getString(audioModeName(mode)))
            } else {
                getString(R.string.audio_unavailable)
            }
            runOnUiThread {
                audioMode = mode
                window.decorView.announceForAccessibility(message)
            }
        }

        override fun onMic(mode: MicStreamMode, available: Boolean) {
            val name = getString(micModeName(mode))
            val live = mode == MicStreamMode.UNMUTED || mode == MicStreamMode.VAD
            val message = when {
                !available -> getString(R.string.mic_unavailable)
                live && !micPermitted -> getString(R.string.mic_mode_now_no_permission, name)
                else -> getString(R.string.mic_mode_now, name)
            }
            runOnUiThread {
                if (available) micMode = mode else micCreated = false
                window.decorView.announceForAccessibility(message)
            }
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
        recordingCallback?.let {
            (getSystemService(Context.AUDIO_SERVICE) as AudioManager).unregisterAudioRecordingCallback(it)
        }
        recordingCallback = null
    }

    /**
     * The phone's own microphone. The app always records from it, even with a headset
     * connected: a Bluetooth headset's microphone would switch the headset to its call
     * profile, with narrowband audio for everything, speech included.
     */
    private fun builtInMicrophone(): Int? =
        (getSystemService(Context.AUDIO_SERVICE) as AudioManager)
            .getDevices(AudioManager.GET_DEVICES_INPUTS)
            .firstOrNull { it.type == AudioDeviceInfo.TYPE_BUILTIN_MIC }
            ?.id

    /**
     * Announces when Android gives the microphone to another app, such as a phone call,
     * and when it comes back. Android reports only this app's own recordings to it.
     */
    private fun watchMicrophoneSilencing() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return
        val audio = getSystemService(Context.AUDIO_SERVICE) as AudioManager
        var silenced = false
        val callback = object : AudioManager.AudioRecordingCallback() {
            override fun onRecordingConfigChanged(configs: List<AudioRecordingConfiguration>) {
                val now = configs.any { it.isClientSilenced }
                if (now == silenced) return
                silenced = now
                window.decorView.announceForAccessibility(
                    getString(if (now) R.string.mic_silenced else R.string.mic_resumed),
                )
            }
        }
        audio.registerAudioRecordingCallback(callback, null)
        recordingCallback = callback
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

    override fun endSession() = end()

    override fun cycleAudioMode() {
        // A mode outside the cycle (e.g. after the computer refused) continues at the start.
        val next = audioCycle[(audioCycle.indexOf(audioMode) + 1) % audioCycle.size]
        audioMode = next
        session?.setAudioMode(next)
    }

    override fun cycleMicMode() {
        // Past the current mode, skipping Off once the computer has the microphone, unless
        // the user allowed removing it.
        val start = micCycle.indexOf(micMode)
        val next = (1..micCycle.size)
            .map { micCycle[(start + it).mod(micCycle.size)] }
            .firstOrNull { it != MicStreamMode.OFF || !micCreated || micAllowRemove }
            ?: micMode
        micMode = next
        if (next != MicStreamMode.OFF) micCreated = true
        session?.setMicMode(next)
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
