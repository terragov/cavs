// Generates a pair of synthetic builds (v1 and v2) so the CAVS examples
// can run end to end without bringing your own data.
//
// v2 is derived from v1 with a realistic mix of changes: some files stay
// identical, one is patched in place, one is brand new, and one is removed.
// The payloads are large and mostly repetitive on purpose — that is exactly
// the shape CAVS exploits, so the update ends up far smaller than a full
// re-download.
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

export interface Builds {
  v1: string;
  v2: string;
}

/** Write Build_v1 and Build_v2 under root and return their paths. */
export function generate(root: string): Builds {
  const v1 = join(root, "Build_v1");
  const v2 = join(root, "Build_v2");

  const level2 = filler("level-two", 2 * 1024 * 1024);
  const files1: Record<string, Buffer> = {
    "app.exe": filler("engine-core", 512 * 1024),
    "data/level1.pak": filler("level-one", 2 * 1024 * 1024),
    "data/level2.pak": level2,
    "assets/textures.bin": filler("textures", 3 * 1024 * 1024),
    "README.txt": Buffer.from("CAVS demo build v1\n"),
  };
  writeTree(v1, files1);

  // level1.pak + textures.bin: identical (fully reused).
  // app.exe: a small region changed (mostly reused).
  // level2.pak: a tail appended (mostly reused).
  // level3.pak: brand new. README.txt: deleted.
  const files2: Record<string, Buffer> = {
    "app.exe": patch(files1["app.exe"], 4096, "engine-core v2 hotfix"),
    "data/level1.pak": files1["data/level1.pak"],
    "data/level2.pak": Buffer.concat([level2, filler("level-two-dlc", 256 * 1024)]),
    "data/level3.pak": filler("level-three", 2 * 1024 * 1024),
    "assets/textures.bin": files1["assets/textures.bin"],
  };
  writeTree(v2, files2);

  return { v1, v2 };
}

// Deterministic, compressible-but-not-trivial content seeded by tag.
function filler(tag: string, n: number): Buffer {
  const seed = Buffer.from(`[${tag}]-cavs-sample-block-`);
  const out = Buffer.allocUnsafe(n);
  for (let i = 0; i < n; i++) {
    out[i] = seed[i % seed.length];
  }
  return out;
}

// Copy src and overwrite a region at off, modelling a small localized change.
function patch(src: Buffer, off: number, marker: string): Buffer {
  const out = Buffer.from(src);
  out.write(marker, off);
  return out;
}

function writeTree(dir: string, files: Record<string, Buffer>): void {
  for (const [rel, data] of Object.entries(files)) {
    const full = join(dir, rel);
    mkdirSync(dirname(full), { recursive: true });
    writeFileSync(full, data);
  }
}

/** Format a byte count as a human-readable string. */
export function human(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let n = bytes / 1024;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(1)} ${units[i]}`;
}
