package com.rastislavkish.rconnect

import android.view.KeyEvent
import uniffi.rconnect_ffi.SessionHandle

/**
 * Sends key events to the active session. Both the session screen and the accessibility
 * service feed it; the service sees keys the system would otherwise keep for itself.
 * Called on the main thread only.
 */
object KeyRouter {
    private var session: SessionHandle? = null
    private var onEndShortcut: (() -> Unit)? = null
    /** Keys whose press was sent, so each release matches a press. */
    private val pressed = mutableSetOf<Int>()

    val active: Boolean
        get() = session != null

    fun attach(session: SessionHandle, onEndShortcut: () -> Unit) {
        this.session = session
        this.onEndShortcut = onEndShortcut
        pressed.clear()
    }

    fun detach(session: SessionHandle) {
        if (this.session === session) {
            this.session = null
            onEndShortcut = null
            pressed.clear()
        }
    }

    /** Returns true when the event was consumed by the session. */
    fun handle(event: KeyEvent): Boolean {
        val session = session ?: return false
        val code = KeyCodes.evdev(event) ?: return true
        when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                // The computer repeats held keys itself.
                if (event.repeatCount > 0) return true
                if (code == KeyCodes.KEY_F11 && KeyCodes.KEY_CAPSLOCK in pressed) {
                    session.releaseAll()
                    pressed.clear()
                    onEndShortcut?.invoke()
                    return true
                }
                pressed += code
                session.sendKey(code.toUShort(), true)
            }
            KeyEvent.ACTION_UP -> {
                if (pressed.remove(code)) session.sendKey(code.toUShort(), false)
            }
        }
        return true
    }
}
