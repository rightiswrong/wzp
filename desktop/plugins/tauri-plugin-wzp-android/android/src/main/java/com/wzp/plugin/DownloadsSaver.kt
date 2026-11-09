package com.wzp.plugin

import android.content.ContentValues
import android.content.Context
import android.media.MediaScannerConnection
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import androidx.annotation.RequiresApi
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.util.Locale

/**
 * Copies a finished download into the user-visible Downloads/WarzonePhone/.
 *
 * - API 29+: MediaStore.Downloads with RELATIVE_PATH and IS_PENDING (no
 *   permission needed).
 * - API 26-28: the legacy public Downloads dir (needs WRITE_EXTERNAL_STORAGE,
 *   declared with maxSdkVersion=28 and requested at save time); if the user
 *   denies it, the app-specific external dir
 *   Android/data/<pkg>/files/Download/WarzonePhone/ (no permission, visible
 *   in file managers, removed on uninstall).
 */
internal object DownloadsSaver {
  private const val SUBDIR = "WarzonePhone"

  private fun mimeFor(name: String, given: String?): String {
    // Prefer the type implied by the extension so MediaStore doesn't append
    // a different extension to the display name.
    val ext = name.substringAfterLast('.', "").lowercase(Locale.ROOT)
    val byExt = if (ext.isNotEmpty()) MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) else null
    return byExt ?: given?.takeIf { it.contains('/') } ?: "application/octet-stream"
  }

  private fun requireSource(src: File) {
    if (!src.isFile) throw IOException("Source file not found: ${src.path}")
  }

  @RequiresApi(29)
  fun saveMediaStore(context: Context, src: File, rawName: String, mime: String?): String {
    requireSource(src)
    val resolver = context.contentResolver
    val name = FileIo.sanitizeName(rawName)
    val relPath = Environment.DIRECTORY_DOWNLOADS + "/" + SUBDIR + "/"
    @Suppress("DEPRECATION")
    val volumeRoot = Environment.getExternalStorageDirectory()
    FileIo.ensureSpace(volumeRoot, src.length(), "\"$name\" in Downloads")

    val collection = MediaStore.Downloads.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)
    // Avoid collisions with entries we can see (our own); MediaProvider
    // de-duplicates against the rest itself.
    var candidate = name
    var n = 1
    while (n < 10000) {
      val exists = resolver.query(
        collection,
        arrayOf(MediaStore.MediaColumns._ID),
        "${MediaStore.MediaColumns.RELATIVE_PATH}=? AND ${MediaStore.MediaColumns.DISPLAY_NAME}=?",
        arrayOf(relPath, candidate),
        null
      )?.use { it.count > 0 } ?: false
      if (!exists) break
      candidate = FileIo.numbered(name, n++)
    }

    val values = ContentValues().apply {
      put(MediaStore.MediaColumns.DISPLAY_NAME, candidate)
      put(MediaStore.MediaColumns.MIME_TYPE, mimeFor(candidate, mime))
      put(MediaStore.MediaColumns.RELATIVE_PATH, relPath)
      put(MediaStore.MediaColumns.IS_PENDING, 1)
    }
    val uri = resolver.insert(collection, values)
      ?: throw IOException("MediaStore refused to create \"$candidate\" in Downloads")
    try {
      val out = resolver.openOutputStream(uri, "w")
        ?: throw IOException("Cannot open \"$candidate\" in Downloads for writing")
      out.use { o -> FileInputStream(src).use { i -> FileIo.copy(i, o, volumeRoot, "\"$candidate\"") } }
      val done = ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }
      resolver.update(uri, done, null, null)
    } catch (e: Exception) {
      try { resolver.delete(uri, null, null) } catch (_: Exception) {}
      throw e
    }
    val finalName = try {
      resolver.query(uri, arrayOf(MediaStore.MediaColumns.DISPLAY_NAME), null, null, null)
        ?.use { if (it.moveToFirst()) it.getString(0) else null }
    } catch (_: Exception) { null } ?: candidate
    return relPath + finalName
  }

  /** API 26-28. [public] = WRITE_EXTERNAL_STORAGE granted. Returns a display location. */
  fun saveLegacy(context: Context, src: File, rawName: String, mime: String?, public: Boolean): String {
    requireSource(src)
    val base: File = if (public) {
      @Suppress("DEPRECATION")
      Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS)
    } else {
      context.getExternalFilesDir(Environment.DIRECTORY_DOWNLOADS)
        ?: throw IOException("External storage is not available")
    }
    val dir = File(base, SUBDIR)
    if (!dir.isDirectory && !dir.mkdirs()) throw IOException("Cannot create $dir")
    val name = FileIo.sanitizeName(rawName)
    FileIo.ensureSpace(dir, src.length(), "\"$name\" in Downloads")

    var dest = File(dir, name)
    var n = 1
    while (!dest.createNewFile()) {
      if (n >= 10000) throw IOException("Too many files named \"$name\" in $dir")
      dest = File(dir, FileIo.numbered(name, n++))
    }
    try {
      FileInputStream(src).use { i -> FileOutputStream(dest).use { o -> FileIo.copy(i, o, dir, "\"${dest.name}\"") } }
    } catch (e: Exception) {
      dest.delete()
      throw e
    }
    MediaScannerConnection.scanFile(context, arrayOf(dest.absolutePath), arrayOf(mimeFor(dest.name, mime)), null)
    return if (public) "${Environment.DIRECTORY_DOWNLOADS}/$SUBDIR/${dest.name}" else dest.absolutePath
  }
}
