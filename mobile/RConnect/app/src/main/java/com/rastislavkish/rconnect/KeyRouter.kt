package com.rastislavkish.rconnect

import android.view.KeyEvent
import uniffi.rconnect_ffi.SessionHandle

/**
 * Sends key events to the active session. Both the session screen and the accessibility
 * service feed it; the service sees keys the system would otherwise keep for itself.
 * Called on the main thread only.
 */
object KeyRouter {
    /** What the session shortcuts do. They are CapsLock plus a function key. */
    interface Shortcuts {
        /** CapsLock+F11. */
        fun endSession()
        /** CapsLock+F10. */
        fun cycleAudioMode()
        /** CapsLock+F9. */
        fun cycleMicMode()
    }

    private var session: SessionHandle? = null
    private var shortcuts: Shortcuts? = null
    /** Keys whose press was sent, so each release matches a press. */
    private val pressed = mutableSetOf<Int>()

    val active: Boolean
        get() = session != null

    fun attach(session: SessionHandle, shortcuts: Shortcuts) {
        this.session = session
        this.shortcuts = shortcuts
        pressed.clear()
    }

    fun detach(session: SessionHandle) {
        if (this.session === session) {
            this.session = null
            shortcuts = null
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
                // The CapsLock press has already reached the computer; the second key is
                // swallowed, and its release is not sent either.
                if (code == KeyCodes.KEY_F11 && KeyCodes.KEY_CAPSLOCK in pressed) {
                    session.releaseAll()
                    pressed.clear()
                    shortcuts?.endSession()
                    return true
                }
                if (code == KeyCodes.KEY_F10 && KeyCodes.KEY_CAPSLOCK in pressed) {
                    shortcuts?.cycleAudioMode()
                    return true
                }
                if (code == KeyCodes.KEY_F9 && KeyCodes.KEY_CAPSLOCK in pressed) {
                    shortcuts?.cycleMicMode()
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
