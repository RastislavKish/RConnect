package com.rastislavkish.rconnect

import android.app.Application
import android.content.Context
import android.os.Build
import android.provider.Settings as SystemSettings
import java.io.File
import java.util.Locale
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import uniffi.rconnect_ffi.AudioSettings
import uniffi.rconnect_ffi.AudioStreamMode
import uniffi.rconnect_ffi.MicStreamMode
import uniffi.rconnect_ffi.RConnect
import uniffi.rconnect_ffi.VoiceSettings
import uniffi.rconnect_ffi.defaultVoiceSettings
import uniffi.rconnect_ffi.initLogging
import uniffi.rconnect_ffi.listVoices

/** Holds the Rust core and the speech data for the whole app. */
class RConnectApp : Application() {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    /** The controller core, started once per process. */
    val core: Deferred<RConnect> by lazy {
        scope.async(Dispatchers.IO) {
            initLogging()
            RConnect.create(filesDir.resolve("core").path, deviceName())
        }
    }

    /** Directory containing `espeak-ng-data`, copied out of the APK on first use. */
    val speechData: Deferred<File> by lazy {
        scope.async(Dispatchers.IO) { SpeechData.prepare(this@RConnectApp) }
    }

    val settings by lazy { AppSettings(this) }

    override fun onCreate() {
        super.onCreate()
        // Start both early, so they are ready when the user connects.
        core.start()
        speechData.start()
    }

    private fun deviceName(): String =
        SystemSettings.Global.getString(contentResolver, SystemSettings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() }
            ?: Build.MODEL
}

val Context.app: RConnectApp
    get() = applicationContext as RConnectApp

/** Name of a microphone mode, as shown and announced. */
fun micModeName(mode: MicStreamMode): Int = when (mode) {
    MicStreamMode.OFF -> R.string.mic_mode_off
    MicStreamMode.MUTED -> R.string.mic_mode_muted
    MicStreamMode.UNMUTED -> R.string.mic_mode_unmuted
    MicStreamMode.VAD -> R.string.mic_mode_vad
}

/** Name of an audio stream mode, as shown and announced. */
fun audioModeName(mode: AudioStreamMode): Int = when (mode) {
    AudioStreamMode.OFF -> R.string.audio_mode_off
    AudioStreamMode.AUTO -> R.string.audio_mode_auto
    AudioStreamMode.ALWAYS -> R.string.audio_mode_always
}

/** User preferences. */
class AppSettings(context: Context) {
    private val preferences = context.getSharedPreferences("settings", Context.MODE_PRIVATE)

    /** Play inaudible noise between utterances to keep Bluetooth headphones awake. */
    var keepAlive: Boolean
        get() = preferences.getBoolean("keep_alive", false)
        set(value) = preferences.edit().putBoolean("keep_alive", value).apply()

    /**
     * All three audio modes in the user's order, each with whether CapsLock+F10 cycles
     * through it.
     */
    var audioModes: List<Pair<AudioStreamMode, Boolean>>
        get() = readModes("audio_modes", ALL_MODES, ::modeKey) ?: run {
            // Before the order existed, a single mode was chosen; it goes first.
            val old = modeFromKey(preferences.getString("audio_mode", null) ?: "off")
                ?: AudioStreamMode.OFF
            (listOf(old) + (ALL_MODES - old)).map { it to true }
        }
        set(value) = writeModes("audio_modes", value, ::modeKey)

    /** The modes CapsLock+F10 cycles through; a session starts with the first. */
    fun audioCycle(): List<AudioStreamMode> = audioModes.filter { it.second }.map { it.first }

    /** Volume of the computer's audio, 0 to 100. */
    var audioVolume: Int
        get() = preferences.getInt("audio_volume", 100)
        set(value) = preferences.edit().putInt("audio_volume", value).apply()

    /** Turn the computer's audio down while speech plays. */
    var audioDucking: Boolean
        get() = preferences.getBoolean("audio_ducking", true)
        set(value) = preferences.edit().putBoolean("audio_ducking", value).apply()

    fun audioSettings() = AudioSettings(audioCycle().first(), audioVolume.toUByte(), audioDucking)

    /**
     * The four microphone modes in the user's order, each with whether CapsLock+F9 cycles
     * through it; a session starts in the first checked one. Stored like [audioModes].
     */
    var micModes: List<Pair<MicStreamMode, Boolean>>
        get() = readModes("mic_modes", ALL_MIC_MODES, ::micKey)
            ?: ALL_MIC_MODES.map { it to (it != MicStreamMode.VAD) }
        set(value) = writeModes("mic_modes", value, ::micKey)

    /** The modes CapsLock+F9 cycles through; a session starts in the first. */
    fun micCycle(): List<MicStreamMode> = micModes.filter { it.second }.map { it.first }

    /**
     * Let CapsLock+F9 remove the computer's microphone during a session. Without this, the
     * shortcut skips Off once the microphone exists, so a call cannot lose it.
     */
    var micAllowRemove: Boolean
        get() = preferences.getBoolean("mic_allow_remove", false)
        set(value) = preferences.edit().putBoolean("mic_allow_remove", value).apply()

    /** A stored mode list, if it is complete and has a checked mode. */
    private fun <T> readModes(key: String, all: List<T>, keyOf: (T) -> String): List<Pair<T, Boolean>>? {
        val stored = preferences.getString(key, null)
            ?.split(',')
            ?.mapNotNull { item ->
                val mode = all.firstOrNull { keyOf(it) == item.removePrefix("-") }
                    ?: return@mapNotNull null
                mode to !item.startsWith("-")
            }
            ?.distinctBy { it.first }
        return stored?.takeIf { it.size == all.size && it.any { mode -> mode.second } }
    }

    /** Stores e.g. `auto,off,-always`, where `-` marks a mode left out. */
    private fun <T> writeModes(key: String, modes: List<Pair<T, Boolean>>, keyOf: (T) -> String) {
        preferences.edit().putString(
            key,
            modes.joinToString(",") { (mode, on) -> (if (on) "" else "-") + keyOf(mode) },
        ).apply()
    }

    private companion object {
        val ALL_MODES = listOf(AudioStreamMode.OFF, AudioStreamMode.AUTO, AudioStreamMode.ALWAYS)

        fun modeKey(mode: AudioStreamMode) = when (mode) {
            AudioStreamMode.OFF -> "off"
            AudioStreamMode.AUTO -> "auto"
            AudioStreamMode.ALWAYS -> "always"
        }

        fun modeFromKey(key: String) = ALL_MODES.firstOrNull { modeKey(it) == key }

        val ALL_MIC_MODES = listOf(
            MicStreamMode.OFF,
            MicStreamMode.MUTED,
            MicStreamMode.UNMUTED,
            MicStreamMode.VAD,
        )

        fun micKey(mode: MicStreamMode) = when (mode) {
            MicStreamMode.OFF -> "off"
            MicStreamMode.MUTED -> "muted"
            MicStreamMode.UNMUTED -> "unmuted"
            MicStreamMode.VAD -> "vad"
        }
    }

    /**
     * The speech settings. Until the user picks a voice, the one matching the phone's
     * language is used, if eSpeak NG has it.
     */
    fun voiceSettings(speechData: File): VoiceSettings {
        val defaults = defaultVoiceSettings()
        return VoiceSettings(
            voice = preferences.getString("voice", null) ?: localeVoice(speechData) ?: defaults.voice,
            variant = preferences.getString("variant", null),
            rate = preferences.getInt("rate", defaults.rate.toInt()).toUByte(),
            rateBoost = preferences.getBoolean("rate_boost", defaults.rateBoost),
            pitch = preferences.getInt("pitch", defaults.pitch.toInt()).toUByte(),
            volume = preferences.getInt("volume", defaults.volume.toInt()).toUByte(),
        )
    }

    fun setVoiceSettings(settings: VoiceSettings) {
        preferences.edit()
            .putString("voice", settings.voice)
            .putString("variant", settings.variant)
            .putInt("rate", settings.rate.toInt())
            .putBoolean("rate_boost", settings.rateBoost)
            .putInt("pitch", settings.pitch.toInt())
            .putInt("volume", settings.volume.toInt())
            .apply()
    }

    private fun localeVoice(speechData: File): String? {
        val locale = Locale.getDefault()
        val ids = listVoices(speechData.path).map { it.id }.toSet()
        val tag = locale.toLanguageTag().lowercase()
        return listOf(tag, locale.language.lowercase()).firstOrNull { it in ids }
    }
}
