# NXS format v2 — extensible archive format

## Why v2

The previous format (`NXS6\n` v1) coupled the wire format to specific
features (single LZMA stream, monolithic solid block). Adding a new
feature required bumping the magic and breaking every existing
`.nxs6` file. This v2 fixes that with an **extension registry**:

- A `.nxs6` file is now a sequence of `extension`s, each with an ID
  and a length-prefixed payload.
- Old readers skip unknown extension IDs gracefully.
- New features land as new extension IDs — **no format version
  bumps**, no migration tools, no `.nxs7`/`.nxs8`/`.nxs9` chain.

This mirrors how PNG chunks, ZIP64 extra fields, zstd dictionaries,
JPEG markers, and CBOR major types all evolve without breaking
older files.

## Wire format

```
┌────────────────────────────────────────────────────────────┐
│ HEADER (8 bytes, fixed)                                    │
│   magic[4]    = "NXS\x00"     (0x4E 0x58 0x53 0x00)       │
│   version[1]  = 1             (this document's version)   │
│   flags[1]    = 0             (bit 0 reserved; bit 1–7 TBD) │
│   reserved[2] = 0                                             │
├────────────────────────────────────────────────────────────┤
│ MANIFEST                                                    │
│   n_extensions (u16 LE)                                     │
│   For each extension (in any order):                        │
│     ext_id    (u16 LE)                                      │
│     ext_size  (u32 LE)     size of ext_payload in bytes     │
│     ext_payload (ext_size bytes)                           │
├────────────────────────────────────────────────────────────┤
│   ... arbitrary other extensions ...                        │
└────────────────────────────────────────────────────────────┘
```

No payload is required to live at any specific offset — readers
must walk the manifest linearly and read each extension's payload
by its `ext_size`. Unknown `ext_id`s are skipped (forward `ext_size`
bytes).

## Extension IDs

The registry is **append-only** — once an ID is assigned, its meaning
must not change. New IDs go at the next free slot.

| ID (u16) | Name | Required? | Purpose |
|---|---|---|---|
| `0x0001` | `EXT_TOC` | **Yes** | File entries (name, sizes, preprocessor, chunk_id, chunk offset) |
| `0x0002` | `EXT_PAYLOAD` | **Yes** | The compressed bytes (single LZMA stream or chunked) |
| `0x0010` | `EXT_CHUNKED` | No | `EXT_PAYLOAD` is split into independent chunks |
| `0x0020` | `EXT_DICT_TRAINING` | No | Trained dictionary (256 KB) + training metadata |
| `0x0030` | `EXT_DEDUP_TABLE` | No | Dedup'd chunks → source chunk id (same content) |
| `0x0040` | `EXT_RANDOM_CHUNKS` | No | Bitmask of chunks stored uncompressed (high entropy) |
| `0x0050` | `EXT_PARITY_SHARDS` | No | Per-chunk Reed-Solomon parity shards |
| `0x0060` | `EXT_ENSEMBLE_BACKENDS` | No | Per-chunk backend choice (LZMA / zstd / bzip2) |
| `0x0070` | `EXT_COMPRESSION_MODE` | No | Per-file mode hint for reproducibility |
| `0x0080`–`0xFFFE` | (reserved) | — | Future use |
| `0xFFFF` | (test sentinel) | No | Used by tests to verify unknown-extension skip |

## Required extension: `EXT_TOC` (0x0001)

Payload layout (entries concatenated, no separator):

```
0x0100_v2_TOC_Payload:
    n_entries       (u32 LE)
    For each entry:
      name_len      (u16 LE)
      name          (name_len bytes, UTF-8, no NUL)
      orig_size     (u64 LE)
      pre_size      (u64 LE)
      preproc       (u8)        0=raw 1=conservative 2=swc
      chunk_id      (u8)        0xFF = single-stream; 0..N-1 = chunk N
      chunk_offset  (u64 LE)    offset WITHIN the (decompressed) chunk
```

`chunk_id` semantics (added Sprint 5.7.x+ for `EXT_CHUNKED`):
- `0xFF` = single-stream payload (no `EXT_CHUNKED` extension). The
  `chunk_offset` is into the single LZMA-decompressed byte stream.
- `0..=N-1` = the file lives in chunk N's decompressed bytes, with
  `chunk_offset` as the offset within that specific chunk. The total
  number of chunks `N` is declared in the `EXT_CHUNKED` extension.

## Optional extension: `EXT_CHUNKED` (0x0010)

Lands when the writer splits the compressed payload into N
independent LZMA streams (one per chunk). This unlocks:

- **Random-access extraction.** The decoder only LZMA-decompresses
  the chunks that hold the requested files, instead of the whole
  payload. A user extracting one file from a 1000-file archive pays
  O(its-own-chunk) work, not O(total-archive).
- **Parallel LZMA compression.** Each chunk is compressed on its
  own thread (via rayon's `par_iter`); typical speedup 2-4× on
  multi-core machines.
- **Bounded decoder memory.** The decoder processes one chunk at a
  time (peak ≈ largest chunk's uncompressed size), regardless of
  total archive size.

Payload layout:

```
0x0010_v2_EXT_CHUNKED_Payload:
    n_chunks               (u16 LE)
    For each chunk:
      uncompressed_size    (u64 LE)   bytes after preprocessing
      compressed_size      (u64 LE)   bytes in EXT_PAYLOAD for this chunk
```

When `EXT_CHUNKED` is present, `EXT_PAYLOAD` is the concatenation of
N independently-compressed LZMA streams. Chunk K's bytes live at
`EXT_PAYLOAD[sum(compressed_size[0..K]) .. sum(compressed_size[0..K+1])]`.

**Trade-off:** within-chunk LZMA locality is preserved (each chunk
gets SimHash-sorted internally), but cross-chunk matches are lost
(the dictionary resets at each chunk boundary). Typical cost: 1-3%
ratio on the first chunk boundary, less on subsequent ones. Acceptable
in exchange for the random-access + parallel wins.

**Backward compatibility:** writers using `EXT_CHUNKED` produce
archives that legacy single-stream readers cannot decode (they'd see
EXT_PAYLOAD as a corrupt LZMA stream — clear error). This is
acceptable for now since v2 has no deployed readers yet (the
extension was shipped in the same session it was written).

## Optional extension: `EXT_DEDUP_TABLE` (0x0030)

Lands when the writer detects two or more chunks with byte-identical
uncompressed content (via SHA-256 of each chunk's preprocessed
bytes). The dedup table tells the decoder "chunk K has identical
bytes to chunk M (the source)" — the smart decoder then reuses the
source's already-decompressed bytes for K instead of running LZMA
on K's bytes (which would just produce the same bytes again).

**Big wins on real corpora:**
- `node_modules` directories where the same library gets copied
  multiple times → 70%+ dedup typical
- `.git/objects/` with many duplicate packfiles
- Backups where the same files are repeated in different directories
- Any corpus with structural repetition

**Payload layout:**
```
0x0030_v2_EXT_DEDUP_TABLE_Payload:
    n_dedup           (u16 LE)
    For each dedup:
      duplicate_id    (u16 LE)   // chunk K whose bytes are dedup'd
      source_id       (u16 LE)   // chunk M (NOT itself dedup'd)
```

**Constraints:**
- `duplicate_id != source_id` (no self-dedup — rejected at parse time)
- `source_id` MUST point at a non-dedup'd chunk (no chains, no cycles;
  the writer enforces this by always pointing at the FIRST chunk
  with a given hash)

**On-disk layout:** the duplicate chunk's LZMA bytes are STILL
present in `EXT_PAYLOAD` (as a fallback for readers without dedup
support). What the dedup table actually saves is the decoder's CPU
work (one LZMA decompress per duplicate) and the in-RAM bytes that
flow through. Old readers pay full cost; smart readers pay nothing
for duplicates.

**When the table is empty** (no duplicates detected), the writer
omits the extension entirely — zero overhead for archives that don't
benefit.

## Required extension: `EXT_PAYLOAD` (0x0002)

The compressed bytes. Without `EXT_CHUNKED` the payload is a
single LZMA2 stream (`xz2`-encoded) of the concatenated
preprocessed file bytes. With `EXT_CHUNKED` (extension `0x0010`,
landed in a later sprint) the payload is a sequence of
independently-compressed chunks.

## Reader behavior

```
1. Read 8 bytes. Verify magic == "NXS\x00" and version == 1.
2. Read n_extensions (u16).
3. For each: read ext_id, ext_size, ext_payload (skip bytes for
   unknown IDs).
4. Verify both EXT_TOC and EXT_PAYLOAD are present; error if not.
5. Detect EXT_CHUNKED. If present, the payload is N concatenated
   LZMA streams — decode each independently, then concat the
   uncompressed bytes for legacy callers OR index per-chunk for
   random-access extract (extract_files_chunked).
6. Walk EXT_TOC entries; for each, slice chunk_offset..chunk_offset +
   pre_size from the appropriate decompressed chunk.
7. Apply preprocessor in reverse (where possible — SwcAst is lossy
   and returns the preprocessed form, not the original).
```

## Writer behavior

### Single-stream mode (legacy path, small corpora)

```
1. Preprocess every file. Build solid_uncompressed =
   concat( preprocessor(file_i, file_bytes_i) for i in 0..n ).
2. Sort files by SimHash (text first, random at the end).
3. LZMA-compress solid_uncompressed → lzma_payload.
4. Serialize TOC entries with chunk_id = 0xFF → toc_payload.
5. Assemble extensions = [EXT_TOC, EXT_PAYLOAD].
6. Emit header + manifest.
```

### Chunked mode (`EXT_CHUNKED`, default for v2 since Sprint 5.7.x+)

```
1. Preprocess every file (parallel via rayon). Record each file's
   preprocessed byte count.
2. Group files into chunks based on cumulative preprocessed size
   (default budget: 128 MB per chunk). A file larger than the budget
   gets its own chunk.
3. SimHash-sort files within each chunk.
4. Per chunk (parallel via rayon):
     - Concat preprocessed bytes.
     - LZMA-compress (own XzEncoder instance).
5. Concat all chunks' LZMA bytes → lzma_payload.
6. Build chunk_table = [(uncompressed_size, compressed_size)] for
   each chunk.
7. Serialize TOC entries with chunk_id = chunk index (0..N-1) and
   chunk_offset = within-chunk offset.
8. Assemble extensions = [EXT_TOC, EXT_CHUNKED, EXT_PAYLOAD].
9. Emit header + manifest.
```
```

## Compatibility matrix

| File shape | Old reader (NXS6\n only) | New reader |
|---|---|---|
| Old `NXS6\n` v1 (NXAR files, etc.) | ✅ reads | ✅ reads via legacy dispatch |
| New `NXS\x00` w/ only required extensions | ❌ magic mismatch | ✅ reads |
| New `NXS\x00` w/ optional extensions (CHUNKED, DICT, etc.) | ❌ magic mismatch | ✅ reads, uses extensions |
| New `NXS\x00` w/ unknown future extension | ❌ magic mismatch | ✅ reads, skips unknown |

Future sprints land `EXT_CHUNKED`, `EXT_DICT_TRAINING`, etc. as
**new extension IDs** — never as format version bumps. Old readers
always continue to work (modulo feature degradation when they
hit an unknown extension).

## Endianness

All multi-byte fields are **little-endian** on disk. This is the
same convention as the legacy v1 format and matches x86/ARM-LE.

## Reserved ranges

- `0x8000`–`0xFFFE`: reserved for vendor / third-party extensions.
  Don't assign without a registry update.
- `0xFFFF`: test sentinel. May appear in unit tests; readers must
  skip it (and any other unknown ID) the same way.