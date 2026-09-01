# cavs-object

Content-addressed structural objects for CAVS.

CAVS-1 stores chunks of payload. This generalises that into an object graph:
alongside chunks, a store holds small immutable objects — trees, commits,
indexes, whatever a consumer defines — each naming the objects it depends on.

The split of responsibility is deliberate. CAVS knows ids, dependencies,
reachability and transfer. It does not know what a commit means; that belongs
to the layer above, which stores its schema in the opaque body of an object.

## What is here

| Module | What it does |
| --- | --- |
| `id` | Object classes and domain-separated identity |
| `envelope` | The canonical wire form, and a decoder that rejects every other |
| `store` | A directory of objects, atomic and self-describing |
| `walk` | Reachability, budgeted against a hostile graph |
| `negotiate` | Capabilities, and have-sets exact or probabilistic |
| `bundle` | A verifiable subgraph in one file |
| `sign` | Ed25519 over roots and bundles |
| `promise` | Objects that are absent on purpose |
| `gc` | Deleting what nothing can reach, and nothing else |

## Identity

```text
structural: blake3("cavs-object-v1" ‖ kind ‖ canonical_bytes)
chunk:      blake3(payload)
```

Chunks are the deliberate exception. Their id is exactly what CAVS-1 already
writes, so every manifest and packfile in an existing store keeps naming the
same bytes. Everything else is domain-separated by class, so the same bytes
filed under two classes are two different objects and a decoder cannot be
steered into reading a tree as a commit.

## Three properties worth knowing

**A walk never opens a body.** A chunk has no dependencies by construction, so
the store answers a payload lookup from its 14-byte header and the file size.
Walking 64 MiB of payload measures 5.3 ms against 48.6 ms to read it — and that
is warm cache, where the gap is at its narrowest.

**Durability is batched.** An object is named by its own hash, so a torn write
is detectable and recoverable. What must be ordered is not each object against
a power cut but every object against the reference that names it: write,
`flush`, then publish. Per-object fsync made a handful of small writes take
26 ms; batched, 0.96 ms.

**Absence has three meanings.** Here, promised, or missing — and only the last
is a problem. Without that distinction a metadata-only clone is indistinguishable
from a damaged store.

## Bundles

```bash
cavs bundle create --root <object-id> --out repo.cavsbundle
cavs bundle inspect repo.cavsbundle
cavs bundle verify repo.cavsbundle
cavs bundle import ./store repo.cavsbundle
```

Nothing is published until all of it verifies: the footer, then the table, then
every object against the id it is filed under, and only then is anything
written. A truncated or edited bundle fails with the store byte-identical to
before. Importing twice is importing once.
