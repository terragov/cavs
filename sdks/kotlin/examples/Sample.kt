// Generates a pair of synthetic builds (v1 and v2) so the CAVS examples
// can run end to end without bringing your own data.
//
// v2 is derived from v1 with a realistic mix of changes: some files stay
// identical, one is patched in place, one is brand new, and one is removed.
// The payloads are large and mostly repetitive on purpose — that is exactly
// the shape CAVS exploits, so the update ends up far smaller than a full
// re-download.
package com.cavs.examples

import java.nio.file.Files
import java.nio.file.Path

data class Builds(val v1: Path, val v2: Path)

/** Write Build_v1 and Build_v2 under root and return their paths. */
fun generateBuilds(root: Path): Builds {
    val v1 = root.resolve("Build_v1")
    val v2 = root.resolve("Build_v2")

    val level2 = filler("level-two", 2 * 1024 * 1024)
    val files1 = mapOf(
        "app.exe" to filler("engine-core", 512 * 1024),
        "data/level1.pak" to filler("level-one", 2 * 1024 * 1024),
        "data/level2.pak" to level2,
        "assets/textures.bin" to filler("textures", 3 * 1024 * 1024),
        "README.txt" to "CAVS demo build v1\n".toByteArray(),
    )
    writeTree(v1, files1)

    // level1.pak + textures.bin: identical (fully reused).
    // app.exe: a small region changed (mostly reused).
    // level2.pak: a tail appended (mostly reused).
    // level3.pak: brand new. README.txt: deleted.
    val files2 = mapOf(
        "app.exe" to patch(files1["app.exe"]!!, 4096, "engine-core v2 hotfix"),
        "data/level1.pak" to files1["data/level1.pak"]!!,
        "data/level2.pak" to (level2 + filler("level-two-dlc", 256 * 1024)),
        "data/level3.pak" to filler("level-three", 2 * 1024 * 1024),
        "assets/textures.bin" to files1["assets/textures.bin"]!!,
    )
    writeTree(v2, files2)

    return Builds(v1, v2)
}

// Deterministic, compressible-but-not-trivial content seeded by tag.
private fun filler(tag: String, n: Int): ByteArray {
    val seed = "[$tag]-cavs-sample-block-".toByteArray()
    return ByteArray(n) { i -> seed[i % seed.size] }
}

// Copy src and overwrite a region at off, modelling a small localized change.
private fun patch(src: ByteArray, off: Int, marker: String): ByteArray {
    val out = src.copyOf()
    val bytes = marker.toByteArray()
    for (i in bytes.indices) {
        if (off + i < out.size) out[off + i] = bytes[i]
    }
    return out
}

private fun writeTree(dir: Path, files: Map<String, ByteArray>) {
    for ((rel, data) in files) {
        val full = dir.resolve(rel)
        Files.createDirectories(full.parent)
        Files.write(full, data)
    }
}

/** Format a byte count as a human-readable string. */
fun human(bytes: Long): String {
    if (bytes < 1024) return "$bytes B"
    val units = listOf("KiB", "MiB", "GiB", "TiB")
    var n = bytes.toDouble() / 1024
    var i = 0
    while (n >= 1024 && i < units.size - 1) {
        n /= 1024
        i++
    }
    return "%.1f %s".format(n, units[i])
}
