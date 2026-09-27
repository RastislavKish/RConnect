package com.rastislavkish.rconnect

import android.app.Application
import android.content.Context
import android.os.Build
import android.provider.Settings as SystemSettings
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import uniffi.rconnect_ffi.RConnect
import uniffi.rconnect_ffi.initLogging

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

/** User preferences. */
class AppSettings(context: Context) {
    private val preferences = context.getSharedPreferences("settings", Context.MODE_PRIVATE)

    /** Play inaudible noise between utterances to keep Bluetooth headphones awake. */
    var keepAlive: Boolean
        get() = preferences.getBoolean("keep_alive", false)
        set(value) = preferences.edit().putBoolean("keep_alive", value).apply()
}
