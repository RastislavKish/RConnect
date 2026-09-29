package com.rastislavkish.rconnect

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
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
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.unit.dp
import androidx.lifecycle.lifecycleScope
import com.rastislavkish.rconnect.ui.theme.RConnectTheme
import java.io.File
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.rconnect_ffi.AudioStreamMode
import uniffi.rconnect_ffi.MicStreamMode
import uniffi.rconnect_ffi.SpeechPreview
import uniffi.rconnect_ffi.VoiceOption
import uniffi.rconnect_ffi.VoiceSettings
import uniffi.rconnect_ffi.listVariants
import uniffi.rconnect_ffi.listVoices
import uniffi.rconnect_ffi.wordsPerMinute

/** Speech and audio settings, with a button to hear the result. */
class SettingsActivity : ComponentActivity() {

    private companion object {
        /** Slider increment, in percent. */
        const val STEP = 5
    }

    private var speechData: File? = null
    private var settings by mutableStateOf<VoiceSettings?>(null)
    private var voices by mutableStateOf<List<VoiceOption>>(emptyList())
    private var variants by mutableStateOf<List<VoiceOption>>(emptyList())
    private var keepAlive by mutableStateOf(false)
    private var audioModes by mutableStateOf<List<Pair<AudioStreamMode, Boolean>>>(emptyList())
    private var micModes by mutableStateOf<List<Pair<MicStreamMode, Boolean>>>(emptyList())
    private var micAllowRemove by mutableStateOf(false)
    private val micPermission = registerForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        if (!granted) {
            window.decorView.announceForAccessibility(getString(R.string.mic_permission_denied))
        }
    }
    private var audioVolume by mutableStateOf(100)
    private var audioDucking by mutableStateOf(true)
    private var preview: SpeechPreview? = null
    private var resumed = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        keepAlive = app.settings.keepAlive
        audioModes = app.settings.audioModes
        micModes = app.settings.micModes
        micAllowRemove = app.settings.micAllowRemove
        audioVolume = app.settings.audioVolume
        audioDucking = app.settings.audioDucking
        lifecycleScope.launch {
            val data = app.speechData.await()
            speechData = data
            val (loadedVoices, loadedVariants, loadedSettings) = withContext(Dispatchers.IO) {
                Triple(listVoices(data.path), listVariants(data.path), app.settings.voiceSettings(data))
            }
            voices = loadedVoices
            variants = loadedVariants
            settings = loadedSettings
            openPreview()
        }
        setContent {
            RConnectTheme {
                Scaffold { padding -> SettingsScreen(Modifier.padding(padding)) }
            }
        }
    }

    override fun onResume() {
        super.onResume()
        resumed = true
        openPreview()
    }

    /**
     * eSpeak NG runs once per process, so the preview is closed before any other screen,
     * such as a session, can start it.
     */
    override fun onPause() {
        super.onPause()
        resumed = false
        preview?.shutdown()
        preview?.close()
        preview = null
    }

    private fun openPreview() {
        val data = speechData ?: return
        val current = settings ?: return
        if (!resumed || preview != null) return
        preview = try {
            SpeechPreview(data.path, current)
        } catch (e: Exception) {
            null
        }
    }

    private fun update(change: (VoiceSettings) -> VoiceSettings) {
        val current = settings ?: return
        val updated = change(current)
        settings = updated
        app.settings.setVoiceSettings(updated)
    }

    @Composable
    private fun SettingsScreen(modifier: Modifier) {
        var choosingVoice by remember { mutableStateOf(false) }
        var choosingVariant by remember { mutableStateOf(false) }

        Column(
            modifier
                .fillMaxSize()
                .padding(16.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Heading(getString(R.string.speech))
            val current = settings
            if (current == null) {
                Text(getString(R.string.loading))
            } else {
                val voiceName = voices.firstOrNull { it.id == current.voice }?.name ?: current.voice
                OutlinedButton(onClick = { choosingVoice = true }, Modifier.fillMaxWidth()) {
                    Text(getString(R.string.voice_value, voiceName))
                }
                val variantName = current.variant
                    ?.let { id -> variants.firstOrNull { it.id == id }?.name ?: id }
                    ?: getString(R.string.no_variant)
                OutlinedButton(onClick = { choosingVariant = true }, Modifier.fillMaxWidth()) {
                    Text(getString(R.string.variant_value, variantName))
                }

                val wpm = wordsPerMinute(current).toInt()
                PercentSlider(
                    label = getString(R.string.rate),
                    value = current.rate.toInt(),
                    max = 100,
                    state = getString(R.string.rate_state, current.rate.toInt(), wpm),
                ) { value -> update { it.copy(rate = value.toUByte()) } }
                SwitchRow(getString(R.string.rate_boost), current.rateBoost) { value ->
                    update { it.copy(rateBoost = value) }
                }
                PercentSlider(
                    label = getString(R.string.pitch),
                    value = current.pitch.toInt(),
                    max = 100,
                    state = getString(R.string.percent, current.pitch.toInt()),
                ) { value -> update { it.copy(pitch = value.toUByte()) } }
                PercentSlider(
                    label = getString(R.string.volume),
                    value = current.volume.toInt(),
                    max = 100,
                    state = getString(R.string.percent, current.volume.toInt()),
                ) { value -> update { it.copy(volume = value.toUByte()) } }

                Button(
                    onClick = { preview?.speak(current, getString(R.string.sample_text)) },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text(getString(R.string.test_speech)) }
            }

            Heading(getString(R.string.audio))
            SwitchRow(getString(R.string.keep_alive), keepAlive) { value ->
                keepAlive = value
                app.settings.keepAlive = value
            }
            OrderedModeList(
                title = getString(R.string.audio_mode),
                hint = getString(R.string.audio_mode_order),
                modes = audioModes,
                name = { getString(audioModeName(it)) },
                description = { getString(audioModeDescription(it)) },
                onChange = { modes ->
                    audioModes = modes
                    app.settings.audioModes = modes
                },
            )
            PercentSlider(
                label = getString(R.string.audio_volume),
                value = audioVolume,
                max = 100,
                state = getString(R.string.percent, audioVolume),
            ) { value ->
                audioVolume = value
                app.settings.audioVolume = value
            }
            SwitchRow(getString(R.string.audio_ducking), audioDucking) { value ->
                audioDucking = value
                app.settings.audioDucking = value
            }

            Heading(getString(R.string.microphone))
            OrderedModeList(
                title = getString(R.string.mic_mode),
                hint = getString(R.string.mic_mode_order),
                modes = micModes,
                name = { getString(micModeName(it)) },
                description = { getString(micModeDescription(it)) },
                onChange = { modes ->
                    micModes = modes
                    app.settings.micModes = modes
                },
                onChecked = ::onMicModeChecked,
            )
            SwitchRow(getString(R.string.mic_allow_remove), micAllowRemove) { value ->
                micAllowRemove = value
                app.settings.micAllowRemove = value
            }
            Text(
                getString(R.string.mic_allow_remove_description),
                style = MaterialTheme.typography.bodySmall,
            )
        }

        val current = settings
        if (choosingVoice && current != null) {
            ChoiceDialog(
                title = getString(R.string.voice),
                options = voices.map { it.id to it.name },
                selected = current.voice,
                onDismiss = { choosingVoice = false },
            ) { id ->
                choosingVoice = false
                update { it.copy(voice = id!!) }
            }
        }
        if (choosingVariant && current != null) {
            ChoiceDialog(
                title = getString(R.string.variant),
                options = listOf(null to getString(R.string.no_variant)) +
                    variants.map { it.id to it.name },
                selected = current.variant,
                onDismiss = { choosingVariant = false },
            ) { id ->
                choosingVariant = false
                update { it.copy(variant = id) }
            }
        }
    }

    private fun audioModeDescription(mode: AudioStreamMode) = when (mode) {
        AudioStreamMode.OFF -> R.string.audio_mode_off_description
        AudioStreamMode.AUTO -> R.string.audio_mode_auto_description
        AudioStreamMode.ALWAYS -> R.string.audio_mode_always_description
    }

    private fun micModeDescription(mode: MicStreamMode) = when (mode) {
        MicStreamMode.OFF -> R.string.mic_mode_off_description
        MicStreamMode.MUTED -> R.string.mic_mode_muted_description
        MicStreamMode.UNMUTED -> R.string.mic_mode_unmuted_description
        MicStreamMode.VAD -> R.string.mic_mode_vad_description
    }

    /** Asks for the microphone once a mode that uses it is checked. */
    private fun onMicModeChecked(mode: MicStreamMode) {
        val live = mode == MicStreamMode.UNMUTED || mode == MicStreamMode.VAD
        if (live && checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            micPermission.launch(Manifest.permission.RECORD_AUDIO)
        }
    }

    @Composable
    private fun Heading(text: String) {
        Text(
            text,
            style = MaterialTheme.typography.headlineSmall,
            modifier = Modifier.semantics { heading() },
        )
    }

    @Composable
    private fun SwitchRow(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
        Row(
            Modifier
                .fillMaxWidth()
                .toggleable(value = checked, role = Role.Switch, onValueChange = onChange),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(label, Modifier.weight(1f))
            Switch(checked = checked, onCheckedChange = null)
        }
    }

    /** A slider in steps of five, announced as `label` with its state, e.g. "40 percent". */
    @Composable
    private fun PercentSlider(
        label: String,
        value: Int,
        max: Int,
        state: String,
        onChange: (Int) -> Unit,
    ) {
        Column {
            Text("$label: $state")
            Slider(
                value = value.toFloat(),
                onValueChange = { onChange((it / STEP).roundToInt() * STEP) },
                valueRange = 0f..max.toFloat(),
                steps = max / STEP - 1,
                modifier = Modifier.semantics {
                    contentDescription = label
                    stateDescription = state
                },
            )
        }
    }

    /** A list of options with the current one selected, in a dialog. */
    @Composable
    private fun ChoiceDialog(
        title: String,
        options: List<Pair<String?, String>>,
        selected: String?,
        onDismiss: () -> Unit,
        onChoose: (String?) -> Unit,
    ) {
        AlertDialog(
            onDismissRequest = onDismiss,
            title = { Text(title) },
            text = {
                val start = options.indexOfFirst { it.first == selected }.coerceAtLeast(0)
                val listState = rememberLazyListState(initialFirstVisibleItemIndex = start)
                LazyColumn(Modifier.heightIn(max = 480.dp).selectableGroup(), state = listState) {
                    items(options) { (id, name) ->
                        Row(
                            Modifier
                                .fillMaxWidth()
                                .selectable(
                                    selected = id == selected,
                                    role = Role.RadioButton,
                                    onClick = { onChoose(id) },
                                )
                                .padding(vertical = 8.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            RadioButton(selected = id == selected, onClick = null)
                            Text(name, Modifier.padding(start = 8.dp))
                        }
                    }
                }
            },
            confirmButton = {
                TextButton(onClick = onDismiss) { Text(getString(R.string.cancel)) }
            },
        )
    }
}
