//! NXS format v2 — extensible archive container.
//!
//! See `NXS_FORMAT.md` for the full wire-format spec. This module
//! implements the manifest read/write loop (header + n_extensions
//! + per-extension length-prefixed payloads) plus the required
//! `EXT_TOC` and `EXT_PAYLOAD` extension helpers.
//!
//! The reader is **forward-compatible** — it skips unknown extension
//! IDs gracefully by reading their `ext_size` bytes and advancing.
//! Writers add new extension IDs in `NXS_FORMAT.md` (one per
//! future feature, never as a format version bump).

use std::convert::TryInto;

/// Magic for v2 archives: `NXS\x00`.
pub const MAGIC: &[u8; 4] = b"NXS\x00";

/// Format version this module implements. Bumped only for
/// incompatible header changes (none planned — extensions carry
/// all future features).
pub const VERSION: u8 = 1;

/// Fixed header size (magic + version + flags + reserved).
pub const HEADER_SIZE: usize = 8;

// ─────────────────────────────────────────────────────────────────────
//   Extension ID registry
//
//   IDs are u16 and **append-only** — once assigned, the meaning
//   must not change. New features land as new IDs, never as format
//   bumps. See `NXS_FORMAT.md` for the full table.
// ─────────────────────────────────────────────────────────────────────

/// `EXT_TOC` — required. The file-entry table.
pub const EXT_TOC: u16 = 0x0001;
/// `EXT_PAYLOAD` — required. The compressed bytes (single LZMA
/// stream when `EXT_CHUNKED` is absent; N concatenated LZMA streams
/// when `EXT_CHUNKED` is present).
pub const EXT_PAYLOAD: u16 = 0x0002;
/// `EXT_CHUNKED` — optional. When present, `EXT_PAYLOAD` is a
/// concatenation of independent LZMA chunks (each its own xz2
/// stream). The chunk table maps chunk index → uncompressed size +
/// compressed size, letting the decoder locate and decompress
/// individual chunks for random-access extraction.
///
/// **Sprint 5.7.x+ (EXT_CHUNKED, planned):** enables per-chunk
/// dedup (later), per-chunk parity shards (later), and bounded
/// decoder memory (already used by `decompress_entries_streaming`).
pub const EXT_CHUNKED: u16 = 0x0010;
/// `EXT_DEDUP_TABLE` — optional. When present, lists chunk IDs that
/// are **byte-identical** to some earlier chunk (deduplicated).
/// The decoder reads those chunks' bytes from the source chunk's
/// slot in `EXT_PAYLOAD` instead of from their own slot, which
/// points at a copy of the same bytes.
///
/// **Sprint 5.7.x+ (EXT_DEDUP_TABLE, just landed):** the writer
/// hashes each chunk's uncompressed bytes after LZMA-compression,
/// groups identical hashes, and emits one entry per duplicate
/// pointing at the first chunk with that hash. The first chunk's
/// bytes remain in `EXT_PAYLOAD`; duplicates' bytes are NOT
/// removed (kept as a fallback for readers without dedup support).
/// Dedup'd chunks contribute compressed_size in the chunk_table
/// just like normal chunks — they share their slot in the
/// (reassembled) payload, but the dedup table tells the smart
/// decoder to skip the LZMA decompress entirely (it can read the
/// already-decompressed source chunk's bytes instead).
pub const EXT_DEDUP_TABLE: u16 = 0x0030;
/// `EXT_PARITY_SHARDS` — optional. When present, each chunk's
/// compressed bytes are Reed-Solomon sharded into N data + M
/// parity shards. The N data shards live in `EXT_PAYLOAD`
/// (concatenated per chunk); the M parity shards live in
/// this extension. The decoder can recover up to M missing
/// data shards per chunk via the parity.
///
/// **Sprint 5.7.x+ (EXT_PARITY_SHARDS, just landed):** parity
/// shards use the GF(2^8) Reed-Solomon implementation in
/// `crate::galois::codes::ReedSolomon` (Cauchy matrix, supports
/// up to 254 total shards per chunk). Default setting:
/// N=10, M=2 → 20% storage overhead, can recover up to 2
/// missing/corrupted data shards per chunk. Format is the
/// SAME as WinRAR's `.par2` style sidecar but inlined into
/// the archive — no separate file needed.
pub const EXT_PARITY_SHARDS: u16 = 0x0050;
/// `EXT_APK_MANIFEST` — optional (NX6R). Stores per-entry ZIP metadata
/// (method, CRC32, uncompressed size) so APK/Split-APK entries can be
/// unpacked into the solid LZMA2 stream and reconstructed cleanly on mobile.
pub const EXT_APK_MANIFEST: u16 = 0x0060;
/// `EXT_SHA256` — optional (NX6R). Per-entry SHA-256 digests (32 bytes each)
/// verified during streaming decompression.
pub const EXT_SHA256: u16 = 0x0061;

/// Sentinel extension ID used by tests to verify that
/// unknown-extension skipping works.
#[cfg(test)]
pub const EXT_TEST_SENTINEL: u16 = 0xFFFF;

/// One extension block in the manifest. `payload` is opaque to the
/// generic reader; only the typed helpers (`decode_toc`,
/// `decode_payload`) know how to parse specific IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub id: u16,
    pub payload: Vec<u8>,
}

/// Build the full archive byte buffer from a list of extensions.
///
/// Layout:
/// ```text
/// [HEADER (8 bytes)][n_extensions (u16 LE)]
/// [ext_id (u16)][ext_size (u32)][ext_payload (ext_size bytes)] × N
/// ```
pub fn write_archive(extensions: &[Extension]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(
        HEADER_SIZE + 2 + extensions.len() * (2 + 4 + 16),
    );
    // Header: magic + version + flags + reserved.
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(0); // flags (bit 0 reserved; 1-7 TBD)
    out.extend_from_slice(&[0u8; 2]); // reserved (align to 8)
    // Manifest: count + per-extension length-prefixed payload.
    out.extend_from_slice(&(extensions.len() as u16).to_le_bytes());
    for ext in extensions {
        out.extend_from_slice(&ext.id.to_le_bytes());
        out.extend_from_slice(&(ext.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&ext.payload);
    }
    out
}

/// Parse the manifest, returning each extension in input order.
/// Unknown extension IDs are returned as `Extension { id, payload }`
/// so callers can introspect them (or just discard). Callers that
/// only want known extensions should use `find_extension` instead.
pub fn read_archive(bytes: &[u8]) -> Result<Vec<Extension>, String> {
    if bytes.len() < HEADER_SIZE {
        return Err(format!(
            "NXS v2: archive too short ({} < {} bytes)",
            bytes.len(),
            HEADER_SIZE
        ));
    }
    if &bytes[..4] != MAGIC {
        return Err("NXS v2: bad magic (expected 'NXS\\x00')".to_string());
    }
    let version = bytes[4];
    if version != VERSION {
        return Err(format!(
            "NXS v2: unsupported version {} (this build reads v{})",
            version, VERSION
        ));
    }
    // bytes[5] is flags, bytes[6..8] is reserved — we don't
    // currently interpret any flag bits. Skip.
    let mut pos = HEADER_SIZE;
    if pos + 2 > bytes.len() {
        return Err("NXS v2: truncated manifest".to_string());
    }
    let n_extensions = u16::from_le_bytes(
        bytes[pos..pos + 2]
            .try_into()
            .map_err(|_| "NXS v2: bad n_extensions".to_string())?,
    ) as usize;
    pos += 2;
    let mut out = Vec::with_capacity(n_extensions);
    for _ in 0..n_extensions {
        if pos + 6 > bytes.len() {
            return Err("NXS v2: truncated extension header".to_string());
        }
        let ext_id = u16::from_le_bytes(
            bytes[pos..pos + 2]
                .try_into()
                .map_err(|_| "NXS v2: bad ext_id".to_string())?,
        );
        pos += 2;
        let ext_size = u32::from_le_bytes(
            bytes[pos..pos + 4]
                .try_into()
                .map_err(|_| "NXS v2: bad ext_size".to_string())?,
        ) as usize;
        pos += 4;
        if pos + ext_size > bytes.len() {
            // Truncated payload: archive was cut short. For
            // most extensions this is fatal, but `EXT_PAYLOAD`
            // + `EXT_PARITY_SHARDS` can recover from this via
            // Reed-Solomon. We tolerate the truncation by reading
            // what's available (caller is responsible for using
            // parity to fill in the missing bytes).
            let available = bytes.len() - pos;
            out.push(Extension {
                id: ext_id,
                payload: bytes[pos..pos + available].to_vec(),
            });
            // Stop reading — any further extensions are also
            // truncated (or non-existent).
            break;
        }
        out.push(Extension {
            id: ext_id,
            payload: bytes[pos..pos + ext_size].to_vec(),
        });
        pos += ext_size;
    }
    Ok(out)
}

/// Find the first extension with the given ID. Returns `None` if
/// the archive doesn't include that extension (this is the "graceful
/// skip" path — writers don't have to include every optional
/// extension, readers don't have to understand them).
pub fn find_extension<'a>(exts: &'a [Extension], id: u16) -> Option<&'a [u8]> {
    exts
        .iter()
        .find(|e| e.id == id)
        .map(|e| e.payload.as_slice())
}

// ─────────────────────────────────────────────────────────────────────
//   EXT_TOC payload — file-entry table
//
//   The TOC payload is a length-prefixed array of `FileEntry`. Each
//   entry is self-contained (length-prefixed name + fixed-size
//   fields) so readers can stop early on truncated files.
// ─────────────────────────────────────────────────────────────────────

/// One file entry in `EXT_TOC`. Mirrors the legacy v1
/// `solid_archive::FileEntry` but with a renamed `chunk_offset`
/// field — the new name reflects the chunked-architecture future
/// (in single-stream mode the offset is into the single payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub original_size: u64,
    pub pre_size: u64,
    pub preprocessor: u8,
    /// 0-based chunk index. `0xFF` means the file lives in a
    /// single-stream payload (no EXT_CHUNKED). 0..N-1 means the
    /// file is in chunk N's decompressed bytes, with
    /// `chunk_offset` as the offset within that chunk.
    ///
    /// **Sprint 5.7.x+ EXT_CHUNKED**: replaced the v1 reserved
    /// byte (which had to be 0) with this field. Old readers that
    /// reject non-zero reserved bytes will fail to parse archives
    /// that have files in chunks other than 0; we accept that
    /// since v2 was just shipped and there are no deployed v2
    /// readers yet (this session).
    pub chunk_id: u8,
    /// Offset WITHIN the chunk identified by `chunk_id` (or within
    /// the single payload when `chunk_id == 0xFF`).
    pub chunk_offset: u64,
}

/// File entries are variable-length (the name field is N bytes), so
/// the TOC payload uses per-entry length-prefixing. Layout:
/// ```text
/// n_entries (u32 LE)
/// For each entry:
///   name_len     (u16 LE)   1..=65535
///   name         (name_len bytes, UTF-8)
///   orig_size    (u64 LE)
///   pre_size     (u64 LE)
///   preproc      (u8)       0=raw 1=conservative 2=swc
///   chunk_id     (u8)       0xFF = single-stream; 0..N-1 = chunk N
///   chunk_offset (u64 LE)
/// ```
/// Each entry is 27 bytes header + name_len bytes of name.
pub fn serialize_toc(entries: &[FileEntry]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(4 + entries.len() * 32);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        let name_bytes = e.name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            // Same bound as the legacy writer. We keep it as a hard
            // error rather than silently truncating because a
            // 64 KB filename is almost certainly a bug or attack.
            panic!(
                "EXT_TOC: file name too long ({} bytes) — refusing to serialize",
                name_bytes.len()
            );
        }
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&e.original_size.to_le_bytes());
        out.extend_from_slice(&e.pre_size.to_le_bytes());
        out.push(e.preprocessor);
        out.push(e.chunk_id);
        out.extend_from_slice(&e.chunk_offset.to_le_bytes());
    }
    out
}

/// Parse a TOC payload back into file entries.
pub fn deserialize_toc(bytes: &[u8]) -> Result<Vec<FileEntry>, String> {
    if bytes.len() < 4 {
        return Err("EXT_TOC: too short for n_entries".to_string());
    }
    let n_entries = u32::from_le_bytes(
        bytes[..4]
            .try_into()
            .map_err(|_| "EXT_TOC: bad n_entries".to_string())?,
    ) as usize;
    let mut pos = 4;
    let mut out = Vec::with_capacity(n_entries);
    for _ in 0..n_entries {
        if pos + 2 > bytes.len() {
            return Err("EXT_TOC: truncated name_len".to_string());
        }
        let name_len = u16::from_le_bytes(
            bytes[pos..pos + 2]
                .try_into()
                .map_err(|_| "EXT_TOC: bad name_len".to_string())?,
        ) as usize;
        pos += 2;
        if pos + name_len > bytes.len() {
            return Err("EXT_TOC: truncated name".to_string());
        }
        let name = String::from_utf8(bytes[pos..pos + name_len].to_vec())
            .map_err(|_| "EXT_TOC: invalid UTF-8 in name".to_string())?;
        pos += name_len;
        if pos + 8 + 8 + 1 + 1 + 8 > bytes.len() {
            return Err("EXT_TOC: truncated entry fields".to_string());
        }
        let original_size = u64::from_le_bytes(
            bytes[pos..pos + 8]
                .try_into()
                .map_err(|_| "EXT_TOC: bad orig_size".to_string())?,
        );
        pos += 8;
        let pre_size = u64::from_le_bytes(
            bytes[pos..pos + 8]
                .try_into()
                .map_err(|_| "EXT_TOC: bad pre_size".to_string())?,
        );
        pos += 8;
        let preprocessor = bytes[pos];
        pos += 1;
        let chunk_id = bytes[pos];
        pos += 1;
        let chunk_offset = u64::from_le_bytes(
            bytes[pos..pos + 8]
                .try_into()
                .map_err(|_| "EXT_TOC: bad chunk_offset".to_string())?,
        );
        pos += 8;
        out.push(FileEntry {
            name,
            original_size,
            pre_size,
            preprocessor,
            chunk_id,
            chunk_offset,
        });
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────
//   EXT_PAYLOAD payload — the actual compressed bytes
//
//   In Sprint 1 the payload is a single LZMA2 stream (the legacy
//   xz2 encoding). Future sprints (EXT_CHUNKED) will switch this
//   to a chunked container.
// ─────────────────────────────────────────────────────────────────────

/// Wrap an LZMA-compressed byte stream as an `EXT_PAYLOAD` extension.
pub fn payload_from_lzma(lzma_bytes: Vec<u8>) -> Extension {
    Extension {
        id: EXT_PAYLOAD,
        payload: lzma_bytes,
    }
}

// ─────────────────────────────────────────────────────────────────────
//   EXT_APK_MANIFEST (0x0060) & EXT_SHA256 (0x0061) — NX6R extensions
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkEntryMeta {
    pub entry_name: String,
    pub method: u16,
    pub crc32: u32,
    pub uncompressed_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkManifest {
    pub apk_name: String,
    pub entries: Vec<ApkEntryMeta>,
}

pub fn serialize_apk_manifests(manifests: &[ApkManifest]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(manifests.len() as u32).to_le_bytes());
    for m in manifests {
        let apk_b = m.apk_name.as_bytes();
        out.extend_from_slice(&(apk_b.len() as u16).to_le_bytes());
        out.extend_from_slice(&(m.entries.len() as u32).to_le_bytes());
        out.extend_from_slice(apk_b);
        for item in &m.entries {
            let en_b = item.entry_name.as_bytes();
            out.extend_from_slice(&(en_b.len() as u16).to_le_bytes());
            out.extend_from_slice(en_b);
            out.extend_from_slice(&item.method.to_le_bytes());
            out.extend_from_slice(&item.crc32.to_le_bytes());
            out.extend_from_slice(&item.uncompressed_size.to_le_bytes());
        }
    }
    out
}

pub fn deserialize_apk_manifests(bytes: &[u8]) -> Result<Vec<ApkManifest>, String> {
    if bytes.len() < 4 {
        return Err("EXT_APK_MANIFEST: truncated".to_string());
    }
    let n_apks = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    let mut pos = 4;
    let mut out = Vec::with_capacity(n_apks);
    for _ in 0..n_apks {
        if pos + 6 > bytes.len() {
            return Err("EXT_APK_MANIFEST: truncated apk header".to_string());
        }
        let apk_len = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
        let n_items = u32::from_le_bytes(bytes[pos + 2..pos + 6].try_into().unwrap()) as usize;
        pos += 6;
        if pos + apk_len > bytes.len() {
            return Err("EXT_APK_MANIFEST: truncated apk name".to_string());
        }
        let apk_name = String::from_utf8(bytes[pos..pos + apk_len].to_vec())
            .map_err(|_| "EXT_APK_MANIFEST: invalid utf8".to_string())?;
        pos += apk_len;
        let mut entries = Vec::with_capacity(n_items);
        for _ in 0..n_items {
            if pos + 2 > bytes.len() {
                return Err("EXT_APK_MANIFEST: truncated entry len".to_string());
            }
            let en_len = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
            pos += 2;
            if pos + en_len + 14 > bytes.len() {
                return Err("EXT_APK_MANIFEST: truncated entry body".to_string());
            }
            let entry_name = String::from_utf8(bytes[pos..pos + en_len].to_vec())
                .map_err(|_| "EXT_APK_MANIFEST: invalid entry utf8".to_string())?;
            pos += en_len;
            let method = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
            let crc32 = u32::from_le_bytes(bytes[pos + 2..pos + 6].try_into().unwrap());
            let uncompressed_size = u64::from_le_bytes(bytes[pos + 6..pos + 14].try_into().unwrap());
            pos += 14;
            entries.push(ApkEntryMeta {
                entry_name,
                method,
                crc32,
                uncompressed_size,
            });
        }
        out.push(ApkManifest { apk_name, entries });
    }
    Ok(out)
}

pub fn serialize_sha256_table(hashes: &[(String, [u8; 32])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(hashes.len() as u32).to_le_bytes());
    for (path, digest) in hashes {
        let pb = path.as_bytes();
        out.extend_from_slice(&(pb.len() as u16).to_le_bytes());
        out.extend_from_slice(pb);
        out.extend_from_slice(digest);
    }
    out
}

pub fn deserialize_sha256_table(bytes: &[u8]) -> Result<Vec<(String, [u8; 32])>, String> {
    if bytes.len() < 4 {
        return Err("EXT_SHA256: truncated".to_string());
    }
    let n = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    let mut pos = 4;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        if pos + 2 > bytes.len() {
            return Err("EXT_SHA256: truncated path len".to_string());
        }
        let plen = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        if pos + plen + 32 > bytes.len() {
            return Err("EXT_SHA256: truncated entry".to_string());
        }
        let path = String::from_utf8(bytes[pos..pos + plen].to_vec())
            .map_err(|_| "EXT_SHA256: invalid utf8".to_string())?;
        pos += plen;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&bytes[pos..pos + 32]);
        pos += 32;
        out.push((path, digest));
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────
//   EXT_CHUNKED payload — chunk table
//
//   When present, `EXT_PAYLOAD` is the concatenation of N independent
//   LZMA streams (one per chunk). This table tells the decoder how
//   many chunks there are and how big each one is, in both
//   uncompressed (so the decoder knows where each file's
//   `chunk_offset` falls) and compressed (so it can locate each
//   chunk's bytes inside the payload without parsing the LZMA
//   stream headers).
//
//   Layout:
//   ```text
//   n_chunks           (u16 LE)
//   For each chunk:
//     uncompressed_size (u64 LE)
//     compressed_size   (u64 LE)
//   ```
//
//   The decoder computes the i-th chunk's uncompressed start by
//   summing `uncompressed_size` of chunks 0..i, and the i-th
//   chunk's compressed start by summing `compressed_size` of chunks
//   0..i. Then `file.chunk_offset` (from `EXT_TOC`) tells the
//   decoder where the file's bytes are inside its owning chunk.
// ─────────────────────────────────────────────────────────────────────

/// One entry in the EXT_CHUNKED table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    /// Size of this chunk's bytes AFTER preprocessing, BEFORE LZMA.
    pub uncompressed_size: u64,
    /// Size of this chunk's LZMA stream (the bytes that go into
    /// `EXT_PAYLOAD`). Each chunk is a complete xz2 stream so this
    /// includes the xz2 header + footer.
    pub compressed_size: u64,
}

/// Serialize a chunk table.
pub fn serialize_chunk_table(chunks: &[ChunkInfo]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(2 + chunks.len() * 16);
    out.extend_from_slice(&(chunks.len() as u16).to_le_bytes());
    for c in chunks {
        out.extend_from_slice(&c.uncompressed_size.to_le_bytes());
        out.extend_from_slice(&c.compressed_size.to_le_bytes());
    }
    out
}

/// Parse a chunk table back into `ChunkInfo` entries. Validates
/// that every entry has non-zero sizes (a chunk with zero bytes is
/// almost certainly a writer bug).
pub fn deserialize_chunk_table(bytes: &[u8]) -> Result<Vec<ChunkInfo>, String> {
    if bytes.len() < 2 {
        return Err("EXT_CHUNKED: too short for n_chunks".to_string());
    }
    let n_chunks = u16::from_le_bytes(
        bytes[..2]
            .try_into()
            .map_err(|_| "EXT_CHUNKED: bad n_chunks".to_string())?,
    ) as usize;
    let expected = 2 + n_chunks * 16;
    if bytes.len() < expected {
        return Err(format!(
            "EXT_CHUNKED: truncated (need {} bytes for {} chunks, have {})",
            expected,
            n_chunks,
            bytes.len()
        ));
    }
    let mut pos = 2;
    let mut out = Vec::with_capacity(n_chunks);
    for _ in 0..n_chunks {
        let uncompressed_size = u64::from_le_bytes(
            bytes[pos..pos + 8]
                .try_into()
                .map_err(|_| "EXT_CHUNKED: bad uncompressed_size".to_string())?,
        );
        pos += 8;
        let compressed_size = u64::from_le_bytes(
            bytes[pos..pos + 8]
                .try_into()
                .map_err(|_| "EXT_CHUNKED: bad compressed_size".to_string())?,
        );
        pos += 8;
        if uncompressed_size == 0 || compressed_size == 0 {
            return Err(format!(
                "EXT_CHUNKED: chunk {} has zero size (uncomp={}, comp={})",
                out.len(),
                uncompressed_size,
                compressed_size
            ));
        }
        out.push(ChunkInfo {
            uncompressed_size,
            compressed_size,
        });
    }
    Ok(out)
}

/// Build an `EXT_CHUNKED` extension from a chunk table.
pub fn chunk_table_extension(chunks: &[ChunkInfo]) -> Extension {
    Extension {
        id: EXT_CHUNKED,
        payload: serialize_chunk_table(chunks),
    }
}

// ─────────────────────────────────────────────────────────────────────
//   EXT_PARITY_SHARDS payload — Reed-Solomon shard layout + shards
//
//   When present, each chunk's compressed bytes are split into
//   `data_shards` equal pieces (data shards, stored in
//   `EXT_PAYLOAD` contiguously) plus `parity_shards` GF(2^8) Reed-
//   Solomon parity shards (stored in this extension, contiguously).
//   The decoder can recover up to `parity_shards` missing data
//   shards per chunk.
//
//   Payload layout:
//   ```text
//   [serialized ParityShardLayout array, 2 + n_chunks*10 bytes]
//   [concatenated parity shards, n_chunks * parity_shards * parity_shard_size bytes]
//   ```
//
//   The first part is parsed by `deserialize_parity_layouts`; the
//   second part is read directly using the layout's
//   `parity_shards` and `parity_shard_size` fields.
// ─────────────────────────────────────────────────────────────────────

/// Per-chunk Reed-Solomon layout. Same shard size for all chunks
/// in an archive (uniform size makes decoder offsets trivial).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityShardLayout {
    pub data_shards: u8,
    pub parity_shards: u8,
    pub data_shard_size: u32,
    pub parity_shard_size: u32,
    /// Original LZMA-compressed size of this chunk, BEFORE
    /// sharding (i.e., the actual size of the xz stream we need
    /// to feed to the LZMA decoder). The reassembled data
    /// shards are `data_shards * data_shard_size` bytes — any
    /// trailing bytes beyond `lzma_size` are zero-padding added
    /// by the sharding (the last data shard is rounded up to
    /// `data_shard_size`). The decoder truncates the reassembled
    /// bytes to `lzma_size` before LZMA-decompressing so the
    /// xz decoder never sees the padding (which it would
    /// otherwise misinterpret as the start of another stream).
    pub lzma_size: u32,
}

/// Serialize a list of per-chunk parity layouts.
pub fn serialize_parity_layouts(layouts: &[ParityShardLayout]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(2 + layouts.len() * 14);
    out.extend_from_slice(&(layouts.len() as u16).to_le_bytes());
    for l in layouts {
        out.push(l.data_shards);
        out.push(l.parity_shards);
        out.extend_from_slice(&l.data_shard_size.to_le_bytes());
        out.extend_from_slice(&l.parity_shard_size.to_le_bytes());
        out.extend_from_slice(&l.lzma_size.to_le_bytes());
    }
    out
}

/// Parse parity layouts. Validates that each layout has at least
/// 1 data shard and 1 parity shard (otherwise the codec makes no
/// sense) and that data + parity ≤ 254 (Cauchy constraint).
pub fn deserialize_parity_layouts(bytes: &[u8]) -> Result<Vec<ParityShardLayout>, String> {
    if bytes.len() < 2 {
        return Err("EXT_PARITY_SHARDS: too short for n_chunks".to_string());
    }
    let n_chunks = u16::from_le_bytes(
        bytes[..2]
            .try_into()
            .map_err(|_| "EXT_PARITY_SHARDS: bad n_chunks".to_string())?,
    ) as usize;
    let expected = 2 + n_chunks * 14;
    if bytes.len() < expected {
        return Err(format!(
            "EXT_PARITY_SHARDS: truncated (need {} bytes for {} chunks, have {})",
            expected,
            n_chunks,
            bytes.len()
        ));
    }
    let mut pos = 2;
    let mut out = Vec::with_capacity(n_chunks);
    for _ in 0..n_chunks {
        let data_shards = bytes[pos];
        pos += 1;
        let parity_shards = bytes[pos];
        pos += 1;
        let data_shard_size = u32::from_le_bytes(
            bytes[pos..pos + 4]
                .try_into()
                .map_err(|_| "EXT_PARITY_SHARDS: bad data_shard_size".to_string())?,
        );
        pos += 4;
        let parity_shard_size = u32::from_le_bytes(
            bytes[pos..pos + 4]
                .try_into()
                .map_err(|_| "EXT_PARITY_SHARDS: bad parity_shard_size".to_string())?,
        );
        pos += 4;
        let lzma_size = u32::from_le_bytes(
            bytes[pos..pos + 4]
                .try_into()
                .map_err(|_| "EXT_PARITY_SHARDS: bad lzma_size".to_string())?,
        );
        pos += 4;
        if data_shards == 0 {
            return Err("EXT_PARITY_SHARDS: zero data_shards".to_string());
        }
        if parity_shards == 0 {
            return Err("EXT_PARITY_SHARDS: zero parity_shards".to_string());
        }
        let total = data_shards as usize + parity_shards as usize;
        if total > 254 {
            return Err(format!(
                "EXT_PARITY_SHARDS: too many shards ({} data + {} parity > 254)",
                data_shards, parity_shards
            ));
        }
        if data_shard_size == 0 {
            return Err("EXT_PARITY_SHARDS: zero data_shard_size".to_string());
        }
        if parity_shard_size == 0 {
            return Err("EXT_PARITY_SHARDS: zero parity_shard_size".to_string());
        }
        out.push(ParityShardLayout {
            data_shards,
            parity_shards,
            data_shard_size,
            parity_shard_size,
            lzma_size,
        });
    }
    Ok(out)
}

/// Build the full `EXT_PARITY_SHARDS` extension payload: serialized
/// layouts followed by concatenated parity shard bytes (in chunk
/// order, then parity-shard index within each chunk).
///
/// `parity_bytes_per_chunk` is a flat `Vec<Vec<u8>>` of length
/// `n_chunks`, each inner vec holding the M parity shards for that
/// chunk concatenated together.
pub fn serialize_parity_extension(
    layouts: &[ParityShardLayout],
    parity_bytes_per_chunk: &[Vec<u8>],
) -> Vec<u8> {
    assert_eq!(
        layouts.len(),
        parity_bytes_per_chunk.len(),
        "EXT_PARITY_SHARDS: layouts and parity bytes must have one entry per chunk"
    );
    let mut out = serialize_parity_layouts(layouts);
    let parity_total: usize = parity_bytes_per_chunk.iter().map(|v| v.len()).sum();
    out.reserve(out.len() + parity_total);
    for pb in parity_bytes_per_chunk {
        out.extend_from_slice(pb);
    }
    out
}

/// Parse the layouts from a full EXT_PARITY_SHARDS payload. The
/// parity bytes (everything after the layout section) are read
/// directly by the caller using the returned layouts.
pub fn deserialize_parity_extension(
    bytes: &[u8],
) -> Result<(Vec<ParityShardLayout>, &[u8]), String> {
    let layouts = deserialize_parity_layouts(bytes)?;
    // Layout header: 2 bytes (n_chunks u16) + per-chunk
    // (data_shards u8 + parity_shards u8 + data_shard_size u32
    //  + parity_shard_size u32 + lzma_size u32 = 14 bytes).
    let header_size = 2 + layouts.len() * 14;
    Ok((layouts, &bytes[header_size..]))
}

/// Build an `EXT_PARITY_SHARDS` extension with both layouts and
/// parity bytes.
pub fn parity_shards_extension(
    layouts: &[ParityShardLayout],
    parity_bytes_per_chunk: &[Vec<u8>],
) -> Extension {
    Extension {
        id: EXT_PARITY_SHARDS,
        payload: serialize_parity_extension(layouts, parity_bytes_per_chunk),
    }
}

/// Locate the chunk that contains the given uncompressed byte offset,
/// returning `(chunk_index, offset_within_chunk)`. Uses a linear scan
/// — `O(n_chunks)` — which is fine because we expect at most a few
/// dozen chunks per archive (each ≤ 128 MB, archives up to several
/// GB). If we ever go to thousands of chunks we'll switch to binary
/// search here.
pub fn locate_chunk(chunks: &[ChunkInfo], uncompressed_offset: u64) -> (usize, u64) {
    let mut acc: u64 = 0;
    for (i, c) in chunks.iter().enumerate() {
        let next = acc + c.uncompressed_size;
        if uncompressed_offset < next {
            return (i, uncompressed_offset - acc);
        }
        acc = next;
    }
    // Out-of-range; clamp to last chunk's end so callers can detect
    // via `offset_within_chunk == chunk.uncompressed_size`.
    let last = chunks.len().saturating_sub(1);
    (last, chunks[last].uncompressed_size)
}

// ─────────────────────────────────────────────────────────────────────
//   EXT_DEDUP_TABLE payload — chunk deduplication map
//
//   When the writer hashes each chunk's uncompressed bytes after
//   LZMA-compression and finds duplicates, it emits this table to
//   tell the decoder "chunk K has identical bytes to chunk M".
//   The smart decoder can then reuse chunk M's decompressed bytes
//   for chunk K instead of LZMA-decompressing K's bytes again.
//
//   Layout:
//   ```text
//   n_dedup           (u16 LE)
//   For each dedup:
//     duplicate_id    (u16 LE)   // chunk K whose bytes are dup'd
//     source_id       (u16 LE)   // chunk M to actually read from
//   ```
//
//   Each `source_id` MUST reference a chunk that is NOT itself
//   duplicated (no chains, no cycles). The writer enforces this by
//   only ever pointing at the FIRST chunk with a given hash.
// ─────────────────────────────────────────────────────────────────────

/// One dedup entry: chunk `duplicate_id` has identical bytes to
/// chunk `source_id` (which came first in the archive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupEntry {
    pub duplicate_id: u16,
    pub source_id: u16,
}

/// Serialize a dedup table.
pub fn serialize_dedup_table(entries: &[DedupEntry]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(2 + entries.len() * 4);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in entries {
        out.extend_from_slice(&e.duplicate_id.to_le_bytes());
        out.extend_from_slice(&e.source_id.to_le_bytes());
    }
    out
}

/// Parse a dedup table. Validates that every entry has
/// `duplicate_id != source_id` (self-dedup is nonsense).
pub fn deserialize_dedup_table(bytes: &[u8]) -> Result<Vec<DedupEntry>, String> {
    if bytes.len() < 2 {
        return Err("EXT_DEDUP_TABLE: too short for n_dedup".to_string());
    }
    let n_dedup = u16::from_le_bytes(
        bytes[..2]
            .try_into()
            .map_err(|_| "EXT_DEDUP_TABLE: bad n_dedup".to_string())?,
    ) as usize;
    let expected = 2 + n_dedup * 4;
    if bytes.len() < expected {
        return Err(format!(
            "EXT_DEDUP_TABLE: truncated (need {} bytes for {} entries, have {})",
            expected,
            n_dedup,
            bytes.len()
        ));
    }
    let mut pos = 2;
    let mut out = Vec::with_capacity(n_dedup);
    for _ in 0..n_dedup {
        let duplicate_id = u16::from_le_bytes(
            bytes[pos..pos + 2]
                .try_into()
                .map_err(|_| "EXT_DEDUP_TABLE: bad duplicate_id".to_string())?,
        );
        pos += 2;
        let source_id = u16::from_le_bytes(
            bytes[pos..pos + 2]
                .try_into()
                .map_err(|_| "EXT_DEDUP_TABLE: bad source_id".to_string())?,
        );
        pos += 2;
        if duplicate_id == source_id {
            return Err(format!(
                "EXT_DEDUP_TABLE: self-dedup entry (chunk {} points at itself)",
                duplicate_id
            ));
        }
        out.push(DedupEntry {
            duplicate_id,
            source_id,
        });
    }
    Ok(out)
}

/// Build an `EXT_DEDUP_TABLE` extension from a dedup entry list.
pub fn dedup_table_extension(entries: &[DedupEntry]) -> Extension {
    Extension {
        id: EXT_DEDUP_TABLE,
        payload: serialize_dedup_table(entries),
    }
}

// ─────────────────────────────────────────────────────────────────────
//   Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: write + read should give back the same
    /// extensions in the same order with identical payloads.
    #[test]
    fn roundtrip_no_extensions() {
        let bytes = write_archive(&[]);
        assert_eq!(&bytes[..4], MAGIC);
        assert_eq!(bytes[4], VERSION);
        let parsed = read_archive(&bytes).expect("read_archive");
        assert!(parsed.is_empty());
    }

    #[test]
    fn roundtrip_two_required_extensions() {
        let toc = vec![FileEntry {
            name: "src/index.ts".to_string(),
            original_size: 1234,
            pre_size: 567,
            preprocessor: 2, // SwcAst
            chunk_id: 0xFF,  // single-stream (no EXT_CHUNKED in this test)
            chunk_offset: 0,
        }];
        let toc_payload = serialize_toc(&toc);
        let lzma_payload = b"\xfd\x37\x7a\x58\x5a\x00fake lzma stream".to_vec();
        let extensions = vec![
            Extension {
                id: EXT_TOC,
                payload: toc_payload.clone(),
            },
            payload_from_lzma(lzma_payload.clone()),
        ];
        let bytes = write_archive(&extensions);
        let parsed = read_archive(&bytes).expect("read_archive");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], extensions[0]);
        assert_eq!(parsed[1], extensions[1]);
        // TOC round-trips through serialize/deserialize.
        let toc_back = deserialize_toc(find_extension(&parsed, EXT_TOC).unwrap())
            .expect("deserialize_toc");
        assert_eq!(toc_back, toc);
    }

    #[test]
    fn rejects_bad_magic() {
        let bytes = b"not an nxs file".to_vec();
        let err = read_archive(&bytes).unwrap_err();
        assert!(err.contains("bad magic"), "got: {}", err);
    }

    #[test]
    fn rejects_truncated_header() {
        let err = read_archive(b"NX").unwrap_err();
        assert!(err.contains("too short"), "got: {}", err);
    }

    #[test]
    fn truncated_extension_payload_returns_partial() {
        // Sprint 5.7.x+ EXT_PARITY_SHARDS hotfix: read_archive
        // is now tolerant of a truncated final extension payload.
        // This enables Reed-Solomon recovery when an archive
        // was cut short mid-write or partial-transfer corrupted.
        // We read what's available and let the caller decide
        // whether the truncation is recoverable (via parity)
        // or fatal.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(VERSION);
        bytes.push(0);
        bytes.extend_from_slice(&[0u8; 2]);
        bytes.extend_from_slice(&1u16.to_le_bytes()); // n_extensions
        bytes.extend_from_slice(&EXT_TOC.to_le_bytes());
        bytes.extend_from_slice(&1000u32.to_le_bytes()); // declared
        bytes.extend_from_slice(&[0xABu8; 4]); // only 4 of 1000
        let exts = read_archive(&bytes).expect("read_archive accepts truncation");
        assert_eq!(exts.len(), 1);
        assert_eq!(exts[0].id, EXT_TOC);
        // The available bytes are returned, truncated to what we have.
        assert_eq!(exts[0].payload.len(), 4);
        assert_eq!(exts[0].payload, vec![0xABu8; 4]);
    }

    #[test]
    fn toc_roundtrip_preserves_all_fields() {
        let entries = vec![
            FileEntry {
                name: "src/a.ts".to_string(),
                original_size: u64::MAX,
                pre_size: u64::MAX / 2,
                preprocessor: 2,
                chunk_id: 7,
                chunk_offset: u64::MAX - 1,
            },
            FileEntry {
                name: "".to_string(),
                original_size: 0,
                pre_size: 0,
                preprocessor: 0,
                chunk_id: 0,
                chunk_offset: 0,
            },
            FileEntry {
                name: "deep/nested/path with spaces and unicode ñ.txt".to_string(),
                original_size: 1,
                pre_size: 2,
                preprocessor: 1,
                chunk_id: 0xFF,
                chunk_offset: 42,
            },
        ];
        let bytes = serialize_toc(&entries);
        let back = deserialize_toc(&bytes).expect("deserialize_toc");
        assert_eq!(back, entries);
    }

    /// EXT_CHUNKED round-trip: serialize then deserialize returns
    /// identical chunk info. Covers the empty table (n_chunks=0) edge
    /// case + a few real-world chunk sizes.
    #[test]
    fn chunk_table_roundtrip() {
        let cases: Vec<Vec<ChunkInfo>> = vec![
            // Empty: a single-file archive could legitimately have
            // zero chunks (it would still emit no EXT_CHUNKED, but
            // the table parser shouldn't choke on the empty case).
            vec![],
            vec![ChunkInfo {
                uncompressed_size: 1024,
                compressed_size: 512,
            }],
            vec![
                ChunkInfo {
                    uncompressed_size: 128 * 1024 * 1024,
                    compressed_size: 32 * 1024 * 1024,
                },
                ChunkInfo {
                    uncompressed_size: 64 * 1024 * 1024,
                    compressed_size: 16 * 1024 * 1024,
                },
                ChunkInfo {
                    uncompressed_size: 1,
                    compressed_size: 32,
                },
            ],
        ];
        for chunks in cases {
            let bytes = serialize_chunk_table(&chunks);
            let back = deserialize_chunk_table(&bytes).expect("deserialize_chunk_table");
            assert_eq!(back, chunks);
        }
    }

    /// `locate_chunk` finds the correct (chunk_index, in-chunk
    /// offset) for a given uncompressed offset, including edge
    /// cases (offset 0, offset at exact chunk boundary).
    #[test]
    fn locate_chunk_finds_correct_chunk() {
        let chunks = vec![
            ChunkInfo { uncompressed_size: 100, compressed_size: 50 },
            ChunkInfo { uncompressed_size: 200, compressed_size: 80 },
            ChunkInfo { uncompressed_size: 300, compressed_size: 150 },
        ];
        // offset 0 → chunk 0, in-chunk 0
        assert_eq!(locate_chunk(&chunks, 0), (0, 0));
        // offset 99 → chunk 0, in-chunk 99
        assert_eq!(locate_chunk(&chunks, 99), (0, 99));
        // offset 100 → chunk 1, in-chunk 0
        assert_eq!(locate_chunk(&chunks, 100), (1, 0));
        // offset 299 → chunk 1, in-chunk 199
        assert_eq!(locate_chunk(&chunks, 299), (1, 199));
        // offset 300 → chunk 2, in-chunk 0
        assert_eq!(locate_chunk(&chunks, 300), (2, 0));
        // offset 599 → chunk 2, in-chunk 299
        assert_eq!(locate_chunk(&chunks, 599), (2, 299));
        // out-of-range offset (past end) → clamps to last chunk's
        // end (a defensive default; callers should bound-check
        // against their own file data first).
        assert_eq!(locate_chunk(&chunks, 10000), (2, 300));
    }

    /// EXT_DEDUP_TABLE round-trip: serialize then deserialize
    /// returns identical entries. Covers empty table + a few
    /// realistic cases (single dedup, chain of dedups).
    #[test]
    fn dedup_table_roundtrip() {
        let cases: Vec<Vec<DedupEntry>> = vec![
            // Empty: most archives have no dedup (no chunk repeats).
            vec![],
            // Single dedup: chunk 5 points at chunk 2.
            vec![DedupEntry { duplicate_id: 5, source_id: 2 }],
            // Multiple dedups to the same source (common: chunks
            // 3 and 7 both happen to share content with chunk 1).
            vec![
                DedupEntry { duplicate_id: 3, source_id: 1 },
                DedupEntry { duplicate_id: 7, source_id: 1 },
            ],
        ];
        for entries in cases {
            let bytes = serialize_dedup_table(&entries);
            let back = deserialize_dedup_table(&bytes).expect("deserialize_dedup_table");
            assert_eq!(back, entries);
        }
    }

    /// Self-dedup (duplicate_id == source_id) is rejected at parse
    /// time — it's a writer bug or archive corruption.
    #[test]
    fn dedup_table_rejects_self_reference() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u16.to_le_bytes()); // n_dedup
        bytes.extend_from_slice(&5u16.to_le_bytes()); // duplicate_id
        bytes.extend_from_slice(&5u16.to_le_bytes()); // source_id (same!)
        let err = deserialize_dedup_table(&bytes).unwrap_err();
        assert!(err.contains("self-dedup"), "got: {}", err);
    }
}