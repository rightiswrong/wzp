package com.wzp.plugin

import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.util.UUID

/** A file copied into the app cache (mirrors the Rust `PickedFile`). */
internal data class ImportedFile(val path: String, val name: String, val size: Long, val mime: String?) {
  fun toJs(): JSObject {
    val o = JSObject()
    o.put("path", path)
    o.put("name", name)
    o.put("size", size)
    if (mime != null) o.put("mime", mime)
    return o
  }
}

internal fun filesResponse(files: List<ImportedFile>): JSObject {
  val arr = JSArray()
  for (f in files) arr.put(f.toJs())
  val o = JSObject()
  o.put("files", arr)
  return o
}

internal object FileIo {
  const val BUFFER_SIZE = 1 shl 20
  /** Keep at least this much free on the target volume. */
  const val SPACE_MARGIN = 64L shl 20
  private const val SPACE_CHECK_EVERY = 64L shl 20

  fun human(bytes: Long): String {
    if (bytes < 1024) return "$bytes B"
    val units = arrayOf("KiB", "MiB", "GiB", "TiB")
    var v = bytes.toDouble() / 1024
    var i = 0
    while (v >= 1024 && i < units.size - 1) { v /= 1024; i++ }
    return String.format(java.util.Locale.ROOT, "%.1f %s", v, units[i])
  }

  fun ensureSpace(dir: File, needed: Long, what: String) {
    val free = dir.usableSpace
    if (needed + SPACE_MARGIN > free) {
      throw IOException(
        "Not enough free space for $what: need ${human(needed)}, only ${human(free)} free on the device"
      )
    }
  }

  /**
   * Streams [input] to [output] with a 1 MiB buffer (never loads the file in
   * memory). Re-checks free space on [spaceDir] every 64 MiB so a source of
   * unknown size cannot fill the disk. Returns the number of bytes copied.
   */
  fun copy(input: InputStream, output: OutputStream, spaceDir: File, what: String): Long {
    val buf = ByteArray(BUFFER_SIZE)
    var total = 0L
    var sinceCheck = 0L
    while (true) {
      val n = input.read(buf)
      if (n < 0) break
      output.write(buf, 0, n)
      total += n
      sinceCheck += n
      if (sinceCheck >= SPACE_CHECK_EVERY) {
        sinceCheck = 0
        if (spaceDir.usableSpace < SPACE_MARGIN) {
          throw IOException("Ran out of free space while copying $what (${human(total)} copied)")
        }
      }
    }
    output.flush()
    return total
  }

  /** Makes a provider-supplied name safe to use as a single path component. */
  fun sanitizeName(raw: String?, fallback: String = "file"): String {
    var s = (raw ?: "").replace(Regex("[\\u0000-\\u001f/\\\\]"), "_").trim()
    if (s.isEmpty() || s == "." || s == "..") s = fallback
    // Keep well below the usual 255-byte filename limit, preserving the extension.
    while (s.toByteArray(Charsets.UTF_8).size > 200) {
      val dot = s.lastIndexOf('.')
      s = if (dot > 0 && s.length - dot <= 16) {
        s.substring(0, dot - 1) + s.substring(dot)
      } else {
        s.substring(0, s.length - 1)
      }
    }
    return s
  }

  /** "name (n).ext" variant used for de-duplication. */
  fun numbered(name: String, n: Int): String {
    val dot = name.lastIndexOf('.')
    return if (dot > 0) "${name.substring(0, dot)} ($n)${name.substring(dot)}" else "$name ($n)"
  }
}

/** Copies `content://` (or `file://`) URIs into `cacheDir/<bucket>/<uuid>/<name>`. */
internal object FileImporter {
  private class Meta(val name: String, val size: Long, val mime: String?)

  private fun queryMeta(resolver: ContentResolver, uri: Uri, index: Int): Meta {
    var name: String? = null
    var size = -1L
    try {
      resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE), null, null, null)
        ?.use { c ->
          if (c.moveToFirst()) {
            val ni = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            val si = c.getColumnIndex(OpenableColumns.SIZE)
            if (ni >= 0 && !c.isNull(ni)) name = c.getString(ni)
            if (si >= 0 && !c.isNull(si)) size = c.getLong(si)
          }
        }
    } catch (_: Exception) {
      // Some providers don't support queries; fall back to the URI.
    }
    if (name.isNullOrBlank()) name = uri.lastPathSegment?.substringAfterLast('/')
    val mime = try { resolver.getType(uri) } catch (_: Exception) { null }
    return Meta(FileIo.sanitizeName(name, "file-${index + 1}"), size, mime)
  }

  fun import(context: Context, uris: List<Uri>, bucket: String): List<ImportedFile> {
    if (uris.isEmpty()) return emptyList()
    val resolver = context.contentResolver
    val root = File(context.cacheDir, bucket)
    if (!root.isDirectory && !root.mkdirs()) throw IOException("Cannot create cache dir $root")
    val metas = uris.mapIndexed { i, u -> queryMeta(resolver, u, i) }
    val known = metas.sumOf { if (it.size > 0) it.size else 0L }
    FileIo.ensureSpace(root, known, if (metas.size == 1) "\"${metas[0].name}\"" else "${metas.size} files")

    val out = ArrayList<ImportedFile>()
    try {
      for ((i, uri) in uris.withIndex()) {
        val meta = metas[i]
        val dir = File(root, UUID.randomUUID().toString())
        if (!dir.mkdirs()) throw IOException("Cannot create cache dir $dir")
        val dest = File(dir, meta.name)
        try {
          val input = resolver.openInputStream(uri) ?: throw IOException("Cannot open \"${meta.name}\"")
          val n = input.use { inp ->
            FileOutputStream(dest).use { o -> FileIo.copy(inp, o, root, "\"${meta.name}\"") }
          }
          out.add(ImportedFile(dest.absolutePath, meta.name, n, meta.mime))
        } catch (e: Exception) {
          dir.deleteRecursively()
          throw e
        }
      }
    } catch (e: Exception) {
      for (f in out) File(f.path).parentFile?.deleteRecursively()
      throw e
    }
    return out
  }

  /** Stores shared plain text (no file attached) as a .txt file. */
  fun importText(context: Context, text: String, subject: String?, bucket: String): ImportedFile {
    val name = FileIo.sanitizeName(subject?.take(80)?.let { "$it.txt" }, "shared-text.txt")
    val dir = File(File(context.cacheDir, bucket), UUID.randomUUID().toString())
    if (!dir.mkdirs()) throw IOException("Cannot create cache dir $dir")
    val dest = File(dir, name)
    val bytes = text.toByteArray(Charsets.UTF_8)
    dest.writeBytes(bytes)
    return ImportedFile(dest.absolutePath, name, bytes.size.toLong(), "text/plain")
  }
}
