package com.rastislavkish.rconnect

import android.content.Context
import java.io.File

/**
 * eSpeak NG reads its voice data from files, so the data bundled as assets is copied to
 * app storage, again whenever the app is updated.
 */
object SpeechData {
    private const val ASSET_DIR = "espeak-ng-data"

    fun prepare(context: Context): File {
        val root = context.filesDir.resolve("speech")
        val marker = root.resolve("installed-from")
        val version = context.packageManager
            .getPackageInfo(context.packageName, 0)
            .lastUpdateTime
            .toString()
        if (marker.exists() && marker.readText() == version) return root

        root.deleteRecursively()
        copyAssets(context, ASSET_DIR, root.resolve(ASSET_DIR))
        marker.writeText(version)
        return root
    }

    private fun copyAssets(context: Context, path: String, target: File) {
        val children = context.assets.list(path).orEmpty()
        if (children.isEmpty()) {
            target.parentFile?.mkdirs()
            context.assets.open(path).use { input ->
                target.outputStream().use { output -> input.copyTo(output) }
            }
        } else {
            target.mkdirs()
            for (child in children) copyAssets(context, "$path/$child", target.resolve(child))
        }
    }
}
