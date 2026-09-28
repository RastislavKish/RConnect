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

    /** When the computer streams its audio to the phone, at the start of a session. */
    var audioMode: AudioStreamMode
        get() = when (preferences.getString("audio_mode", null)) {
            "auto" -> AudioStreamMode.AUTO
            "always" -> AudioStreamMode.ALWAYS
            else -> AudioStreamMode.OFF
        }
        set(value) = preferences.edit().putString(
            "audio_mode",
            when (value) {
                AudioStreamMode.OFF -> "off"
                AudioStreamMode.AUTO -> "auto"
                AudioStreamMode.ALWAYS -> "always"
            },
        ).apply()

    /** Volume of the computer's audio, 0 to 100. */
    var audioVolume: Int
        get() = preferences.getInt("audio_volume", 100)
        set(value) = preferences.edit().putInt("audio_volume", value).apply()

    /** Turn the computer's audio down while speech plays. */
    var audioDucking: Boolean
        get() = preferences.getBoolean("audio_ducking", true)
        set(value) = preferences.edit().putBoolean("audio_ducking", value).apply()

    fun audioSettings() = AudioSettings(audioMode, audioVolume.toUByte(), audioDucking)

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
