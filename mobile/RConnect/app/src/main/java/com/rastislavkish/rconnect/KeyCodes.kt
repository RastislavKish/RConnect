package com.rastislavkish.rconnect

import android.view.KeyEvent

/** Translation of Android key events to Linux evdev key codes, which the computer uses. */
object KeyCodes {
    const val KEY_CAPSLOCK = 58
    const val KEY_F9 = 67
    const val KEY_F10 = 68
    const val KEY_F11 = 87

    /**
     * Returns the evdev code of a key event, or null when it has none. Physical
     * keyboards report the evdev code as the scan code; the table covers events without
     * one, e.g. from on-screen or remapped keys.
     */
    fun evdev(event: KeyEvent): Int? {
        val scanCode = event.scanCode
        if (scanCode in 1..0x2ff) return scanCode
        return FALLBACK[event.keyCode]
    }

    private val FALLBACK: Map<Int, Int> = buildMap {
        val letters = "qwertyuiop" to 16
        val home = "asdfghjkl" to 30
        val bottom = "zxcvbnm" to 44
        for ((row, first) in listOf(letters, home, bottom)) {
            row.forEachIndexed { i, c -> put(KeyEvent.KEYCODE_A + (c - 'a'), first + i) }
        }
        put(KeyEvent.KEYCODE_0, 11)
        for (digit in 1..9) put(KeyEvent.KEYCODE_0 + digit, 1 + digit)
        for (f in 1..10) put(KeyEvent.KEYCODE_F1 + f - 1, 58 + f)
        put(KeyEvent.KEYCODE_F11, 87)
        put(KeyEvent.KEYCODE_F12, 88)
        put(KeyEvent.KEYCODE_ESCAPE, 1)
        put(KeyEvent.KEYCODE_MINUS, 12)
        put(KeyEvent.KEYCODE_EQUALS, 13)
        put(KeyEvent.KEYCODE_DEL, 14)
        put(KeyEvent.KEYCODE_TAB, 15)
        put(KeyEvent.KEYCODE_LEFT_BRACKET, 26)
        put(KeyEvent.KEYCODE_RIGHT_BRACKET, 27)
        put(KeyEvent.KEYCODE_ENTER, 28)
        put(KeyEvent.KEYCODE_CTRL_LEFT, 29)
        put(KeyEvent.KEYCODE_SEMICOLON, 39)
        put(KeyEvent.KEYCODE_APOSTROPHE, 40)
        put(KeyEvent.KEYCODE_GRAVE, 41)
        put(KeyEvent.KEYCODE_SHIFT_LEFT, 42)
        put(KeyEvent.KEYCODE_BACKSLASH, 43)
        put(KeyEvent.KEYCODE_COMMA, 51)
        put(KeyEvent.KEYCODE_PERIOD, 52)
        put(KeyEvent.KEYCODE_SLASH, 53)
        put(KeyEvent.KEYCODE_SHIFT_RIGHT, 54)
        put(KeyEvent.KEYCODE_ALT_LEFT, 56)
        put(KeyEvent.KEYCODE_SPACE, 57)
        put(KeyEvent.KEYCODE_CAPS_LOCK, 58)
        put(KeyEvent.KEYCODE_NUM_LOCK, 69)
        put(KeyEvent.KEYCODE_SCROLL_LOCK, 70)
        put(KeyEvent.KEYCODE_CTRL_RIGHT, 97)
        put(KeyEvent.KEYCODE_ALT_RIGHT, 100)
        put(KeyEvent.KEYCODE_MOVE_HOME, 102)
        put(KeyEvent.KEYCODE_DPAD_UP, 103)
        put(KeyEvent.KEYCODE_PAGE_UP, 104)
        put(KeyEvent.KEYCODE_DPAD_LEFT, 105)
        put(KeyEvent.KEYCODE_DPAD_RIGHT, 106)
        put(KeyEvent.KEYCODE_MOVE_END, 107)
        put(KeyEvent.KEYCODE_DPAD_DOWN, 108)
        put(KeyEvent.KEYCODE_PAGE_DOWN, 109)
        put(KeyEvent.KEYCODE_INSERT, 110)
        put(KeyEvent.KEYCODE_FORWARD_DEL, 111)
        put(KeyEvent.KEYCODE_META_LEFT, 125)
        put(KeyEvent.KEYCODE_META_RIGHT, 126)
        put(KeyEvent.KEYCODE_MENU, 127)
        put(KeyEvent.KEYCODE_SYSRQ, 99)
        put(KeyEvent.KEYCODE_BREAK, 119)
    }
}
