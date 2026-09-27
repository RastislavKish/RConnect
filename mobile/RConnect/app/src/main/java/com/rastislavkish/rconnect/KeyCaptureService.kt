package com.rastislavkish.rconnect

import android.accessibilityservice.AccessibilityService
import android.content.Context
import android.provider.Settings
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent

/**
 * Filters key events during a session, before the system handles shortcuts such as
 * Alt+Tab or the Meta key. Outside a session every key passes through.
 */
class KeyCaptureService : AccessibilityService() {

    override fun onKeyEvent(event: KeyEvent): Boolean = KeyRouter.handle(event)

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {}

    companion object {
        fun isEnabled(context: Context): Boolean {
            val enabled = Settings.Secure.getString(
                context.contentResolver,
                Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES,
            ) ?: return false
            val name = "${context.packageName}/${KeyCaptureService::class.java.name}"
            return enabled.split(':').any { it.equals(name, ignoreCase = true) }
        }
    }
}
