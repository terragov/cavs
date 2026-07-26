#!/usr/bin/env python3
"""Deterministic versioned datasets for the LFS benchmark harness.

Four scenarios, each a sequence of full version trees under
<root>/<scenario>/v<N>/…, meant to model how large binary assets actually
evolve:

  big-binary    100 MiB incompressible asset; each version edits ~1.25 MiB
                in place and appends 1 MiB (5 versions).
  compressible  64 MiB semi-structured pack (~2-3x zstd-compressible);
                each version replaces ~2% of its blocks (4 versions).
  many-files    250 binary files, log-normal sizes (~90 MiB total); each
                version partially edits 10% of the files (4 versions).
  full-rewrite  48 MiB blob fully rewritten (2 versions) — the honest
                worst case, where chunk dedup cannot help.
  tensor        32 MiB of float32 random-walk "model weights" (numeric,
                BG4-friendly: poorly compressible interleaved, structured
                per byte plane); each version perturbs ~10% of the values
                fine-tune style (3 versions).

Everything is seeded: identical trees on every run/machine.
"""

import array
import itertools
import os
import random
import sys


def wbytes(path: str, data: bytes) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def mutate(data: bytearray, rng: random.Random, n_edits: int, edit_size: int) -> None:
    for _ in range(n_edits):
        off = rng.randrange(0, max(1, len(data) - edit_size))
        data[off : off + edit_size] = rng.randbytes(edit_size)


def scenario_big(root: str) -> None:
    rng = random.Random(42)
    data = bytearray(rng.randbytes(100 * 2**20))
    for v in range(1, 6):
        if v > 1:
            mutate(data, rng, 10, 128 * 1024)
            data += rng.randbytes(2**20)
        wbytes(f"{root}/big-binary/v{v}/asset.bin", bytes(data))


def scenario_compressible(root: str) -> None:
    rng = random.Random(7)

    def block() -> bytes:  # 16 KiB: repeated pattern + noise tail
        pat = rng.randbytes(512)
        return pat * 24 + rng.randbytes(4096)

    blocks = [block() for _ in range(4096)]  # 64 MiB
    for v in range(1, 5):
        if v > 1:
            for _ in range(80):
                blocks[rng.randrange(len(blocks))] = block()
        wbytes(f"{root}/compressible/v{v}/data.pak", b"".join(blocks))


def scenario_many(root: str) -> None:
    rng = random.Random(1337)
    files: dict[str, bytearray] = {}
    for i in range(250):
        size = int(rng.lognormvariate(12.2, 1.1))
        size = max(4096, min(size, 8 * 2**20))
        files[f"assets/f{i:03}.bin"] = bytearray(rng.randbytes(size))
    for v in range(1, 5):
        if v > 1:
            for name in rng.sample(sorted(files), 25):
                mutate(files[name], rng, 4, 32 * 1024)
        for name, data in files.items():
            wbytes(f"{root}/many-files/v{v}/{name}", bytes(data))


def scenario_rewrite(root: str) -> None:
    rng = random.Random(99)
    for v in range(1, 3):
        wbytes(f"{root}/full-rewrite/v{v}/blob.bin", rng.randbytes(48 * 2**20))


def scenario_tensor(root: str) -> None:
    rng = random.Random(2718)
    n = 8 * 2**20  # 8M float32 = 32 MiB
    # Random walk: adjacent values share exponent/high bytes, like real
    # weight/vertex/audio streams — near-incompressible interleaved, but
    # structured per byte plane.
    vals = array.array(
        "f", itertools.accumulate((rng.random() - 0.5 for _ in range(n)), initial=1000.0)
    )
    band = n // 20  # 5% of the tensor per perturbed band
    for v in range(1, 4):
        if v > 1:
            for _ in range(2):  # ~10% of values get small fine-tune deltas
                start = rng.randrange(0, n - band)
                for i in range(start, start + band):
                    vals[i] += rng.random() * 0.01 - 0.005
        wbytes(f"{root}/tensor/v{v}/weights.bin", vals.tobytes())


def main() -> None:
    root = sys.argv[1]
    scenario_big(root)
    scenario_compressible(root)
    scenario_many(root)
    scenario_rewrite(root)
    scenario_tensor(root)
    print(f"datasets ready under {root}")


if __name__ == "__main__":
    main()
