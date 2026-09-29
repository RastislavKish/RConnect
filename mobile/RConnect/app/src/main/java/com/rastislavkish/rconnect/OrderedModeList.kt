package com.rastislavkish.rconnect

import androidx.compose.foundation.gestures.detectDragGesturesAfterLongPress
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.selection.toggleable
import androidx.compose.material3.Checkbox
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.CustomAccessibilityAction
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.customActions
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.compose.ui.zIndex

/**
 * Modes in the user's order, each with a checkbox, as for the shortcuts that cycle through
 * the checked ones; a session starts with the first checked mode. Rows are reordered by
 * dragging after a long press, or with the Move up and Move down accessibility actions,
 * and the new position is announced. At least one mode stays checked.
 *
 * [onChange] receives the whole list after every change, [onChecked] a mode that was
 * just checked.
 */
@Composable
fun <T : Any> OrderedModeList(
    title: String,
    hint: String,
    modes: List<Pair<T, Boolean>>,
    name: (T) -> String,
    description: (T) -> String,
    onChange: (List<Pair<T, Boolean>>) -> Unit,
    onChecked: (T) -> Unit = {},
) {
    val view = LocalView.current
    val haptics = LocalHapticFeedback.current
    // The drag gesture outlives recompositions, so it reads the list through this.
    val current by rememberUpdatedState(modes)
    // The mode being dragged and how far it has moved from its current slot, in pixels.
    var dragged by remember { mutableStateOf<T?>(null) }
    var offset by remember { mutableFloatStateOf(0f) }
    var rowHeight by remember { mutableIntStateOf(1) }
    // A long press that ends without moving still ends in a tap; it must not toggle.
    var ignoreTap by remember { mutableStateOf(false) }
    val keepOne = stringResource(R.string.mode_keep_one)
    val moveUp = stringResource(R.string.move_up)
    val moveDown = stringResource(R.string.move_down)
    val movedFormat = stringResource(R.string.mode_moved)

    fun reorder(from: Int, to: Int): List<Pair<T, Boolean>> {
        val reordered = current.toMutableList()
        reordered.add(to, reordered.removeAt(from))
        onChange(reordered)
        return reordered
    }

    fun announcePosition(mode: T, list: List<Pair<T, Boolean>>) {
        val position = list.indexOfFirst { it.first == mode } + 1
        view.announceForAccessibility(String.format(movedFormat, name(mode), position, list.size))
    }

    fun move(from: Int, to: Int) {
        if (to !in current.indices) return
        announcePosition(current[from].first, reorder(from, to))
    }

    fun setOn(index: Int, on: Boolean) {
        if (!on && current.count { it.second } == 1) {
            view.announceForAccessibility(keepOne)
            return
        }
        onChange(current.toMutableList().also { it[index] = it[index].first to on })
        if (on) onChecked(current[index].first)
    }

    Column {
        Text(title)
        Text(hint, style = MaterialTheme.typography.bodySmall)
        modes.forEachIndexed { index, (mode, on) ->
            key(mode) {
                val actions = buildList {
                    if (index > 0) {
                        add(CustomAccessibilityAction(moveUp) { move(index, index - 1); true })
                    }
                    if (index < modes.size - 1) {
                        add(CustomAccessibilityAction(moveDown) { move(index, index + 1); true })
                    }
                }
                Row(
                    Modifier
                        .fillMaxWidth()
                        .onSizeChanged { rowHeight = it.height.coerceAtLeast(1) }
                        .zIndex(if (dragged == mode) 1f else 0f)
                        .graphicsLayer { translationY = if (dragged == mode) offset else 0f }
                        .pointerInput(mode) {
                            detectDragGesturesAfterLongPress(
                                onDragStart = {
                                    haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                                    ignoreTap = true
                                    dragged = mode
                                    offset = 0f
                                },
                                onDrag = { change, amount ->
                                    change.consume()
                                    offset += amount.y
                                    // Swap with a neighbour once the row is past half of it.
                                    val at = current.indexOfFirst { it.first == mode }
                                    if (offset > rowHeight / 2 && at < current.size - 1) {
                                        reorder(at, at + 1)
                                        offset -= rowHeight
                                    } else if (offset < -rowHeight / 2 && at > 0) {
                                        reorder(at, at - 1)
                                        offset += rowHeight
                                    }
                                },
                                onDragEnd = {
                                    // Cleared after the tap that may follow on this frame.
                                    view.post { ignoreTap = false }
                                    dragged = null
                                    offset = 0f
                                    announcePosition(mode, current)
                                },
                                onDragCancel = {
                                    view.post { ignoreTap = false }
                                    dragged = null
                                    offset = 0f
                                },
                            )
                        }
                        .toggleable(
                            value = on,
                            role = Role.Checkbox,
                            onValueChange = { checked ->
                                if (ignoreTap) ignoreTap = false else setOn(index, checked)
                            },
                        )
                        .semantics { customActions = actions }
                        .padding(vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Checkbox(checked = on, onCheckedChange = null)
                    Column(Modifier.padding(start = 8.dp)) {
                        Text(name(mode))
                        Text(description(mode), style = MaterialTheme.typography.bodySmall)
                    }
                }
            }
        }
    }
}
