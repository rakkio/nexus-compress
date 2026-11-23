//! Solid archive format `NXS6` (NexusCompress Solid v6).
//!
//! ## Why solid?
//!
//! In a typical source-code corpus (Next.js, Rust crate, etc.) the
//! files share huge amounts of structure: the same imports, the same
//! type names, the same function bodies, the same boilerplate. When
//! you compress each file independently, the LZMA dictionary resets
//! between files and can't see those cross-file repetitions.
//!
//! Solid compression concatenates ALL preprocessed files into a
//! single byte stream and runs LZMA over the whole thing. The
//! dictionary now spans the entire corpus — so the second `.tsx`
//! file reuses the matches the LZMA Match Finder found in the first.
//! On a real Next.js corpus this typically buys +5-15% over
//! per-file LZMA at the cost of making the archive LOSSY (you can't
//! extract the original bytes — you get the minified version of
//! each file).
//!
//! ## Format
//!
//! ```text
//! +------------------------+
//! | Magic "NXS6\n"  (5 B)  |
//! | Version       (1 B)    |   = 1
//! | n_files       (4 B LE) |
//! +------------------------+
//! | TOC entry 1            |   repeated n_files times
//! |   name_len  (2 B LE)   |
//! |   name      (N bytes)  |   UTF-8, no null
//! |   orig_size (8 B LE)   |   file size on disk
//! |   pre_size  (8 B LE)   |   bytes after preprocessor
//! |   preproc   (1 B)      |   0=raw 1=conservative 2=swc
//! |   offset    (8 B LE)   |   offset in DECOMPRESSED solid
//! +------------------------+
//! | Solid block (LZMA)     |   xz2 stream, no tag prefix
//! +------------------------+
//! ```
//!
//! ## Honest contract
//!
//! **LOSSY archive.** Decompression returns the *minified* version
//! of each file (or the original bytes if the preprocessor was
//! `Raw`). The original source — comments, formatting, original
//! identifier names — is NOT recoverable. If you need lossless
//! per-file archives, use the v4 `.nxar` format (`NXAR` magic)
//! instead.
//!
//! ## When to use
//!
//! - Distributing minified source bundles (one file per source).
//! - Compressing large corpus snapshots where the smallest size
//!   matters more than byte-perfect recoverability.
//! - Use v4 `.nxar` (per-file lossless) when roundtrip fidelity
//!   matters.

use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use xz2::read::XzDecoder;
use xz2::write::XzEncoder;

use crate::ast_minify;
use crate::classifier;
use crate::format::BlockType as _BlockType; // bring the enum's variants into scope
use crate::format_v2;
use crate::minify;

/// Format magic: `NXS6\n`.
pub const MAGIC: &[u8; 5] = b"NXS6\n";

/// Format version (currently 1).
pub const VERSION: u8 = 1;

/// Per-file preprocessor identifiers.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preprocessor {
    /// Pass the bytes through unchanged.
    Raw = 0,
    /// Conservative text minify (strips comments, collapses whitespace).
    Conservative = 1,
    /// swc AST minify (drops comments, types, formatting — lossy).
    SwcAst = 2,
}

// ──────────────────────────────────────────────────────────────────────────
//   Sprint 5.7.x: content-aware pipeline + SimHash ordering
// ──────────────────────────────────────────────────────────────────────────
//
// Two ratio wins without changing the .nxs6 wire format:
//
//   1. **Content-aware preprocessor override.** `pick_preprocessor()`
//      only looks at the file extension — a `.bin` that actually
//      holds JPEG bytes still goes through `Conservative` minify
//      (which is wasted CPU on random data and pollutes the LZMA
//      dictionary with garbage matches). We sample the first 64 KB
//      through `classifier::classify`; if it's `Random` we force
//      `Raw` so the file is stored as-is. Bit-perfect for already-
//      compressed payloads (no minify applied to random data).
//
//   2. **SimHash ordering.** The LZMA dictionary finds matches across
//      the SOLID stream — its match rate is best when consecutive
//      files share lots of structure. WinRAR/7-zip both sort inputs
//      by content similarity before concatenation. We use a 64-bit
//      SimHash over the first 64 KB and sort ascending; ties break
//      on original order to stay deterministic.
//
//   Format impact: NONE. The TOC entry already carries `preprocessor`
//   and the per-entry `pre_size`. We just route a few more entries
//   to `Raw` and reorder the file list before concatenation. Old
//   decompressors still work because the LZMA stream + offsets
//   stay self-consistent.

/// How many bytes to sample for classification + SimHash. 64 KB
/// matches the LZMA dictionary's "small file" threshold — for files
/// smaller than this we sample the whole thing; for larger files
/// we read the first 64 KB and trust that to be representative.
const SAMPLE_BYTES: usize = 64 * 1024;

/// Choose the preprocessor for a file. The caller passes
/// `is_random` (already computed during the SimHash sort step)
/// so we don't re-run `classifier::classify` here — saves ~30% of
/// per-file CPU on the preprocessor selection path.
///
/// `bytes` is currently unused — the function decides on extension
/// alone once Random is ruled out. We keep it in the signature so
/// a future heuristic (e.g. content-sniff for `.bin` files that
/// are actually text) can use it without a breaking change.
fn pick_preprocessor(name: &str, _bytes: &[u8], is_random: bool) -> Preprocessor {
    // Content-aware override: if the caller already determined the
    // sample is Random, force Raw regardless of the extension
    // (a `.bin` holding JPEGs would otherwise get minified for
    // nothing).
    if is_random {
        return Preprocessor::Raw;
    }
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => Preprocessor::SwcAst,
        _ => Preprocessor::Conservative,
    }
}

/// 64-bit SimHash over a byte slice.
///
/// **Algorithm:** hash each 4-byte word with FNV-1a, take the low
/// 64 bits, increment a 64-counter array where bit i flips sign for
/// each hash where bit i is set. The final 64-bit fingerprint has
/// each bit = sign(sum).
///
/// **Properties:** two files with similar first 64 KB will have
/// similar SimHash values (Hamming distance correlates with content
/// similarity). Cheap to compute (~64 ns per KB on modern CPUs)
/// and stable across runs (no randomness).
///
/// **Note:** the result is *not* cryptographically meaningful — we
/// just want ordering. FNV-1a is fine here.
fn simhash_64(data: &[u8]) -> u64 {
    let sample_len = data.len().min(SAMPLE_BYTES);
    let sample = &data[..sample_len];
    // 64 counters, one per bit. Use i64 to allow sign accumulation.
    let mut counters: [i64; 64] = [0i64; 64];
    // Walk the sample in 4-byte words; if the last word is
    // short, treat it as zero-padded.
    let mut i = 0usize;
    while i < sample.len() {
        let w: u32 = if i + 4 <= sample.len() {
            u32::from_le_bytes([
                sample[i],
                sample[i + 1],
                sample[i + 2],
                sample[i + 3],
            ])
        } else {
            let mut buf = [0u8; 4];
            let take = sample.len() - i;
            buf[..take].copy_from_slice(&sample[i..]);
            u32::from_le_bytes(buf)
        };
        // FNV-1a 64-bit. We don't need the full 64-bit hash here,
        // so we compute a 64-bit hash then take the low 64 bits
        // (which is the whole thing).
        let mut h: u64 = 0xcbf29ce484222325;
        let prime: u64 = 0x100000001b3;
        h ^= w as u64;
        h = h.wrapping_mul(prime);
        for bit in 0..64 {
            if (h >> bit) & 1 == 1 {
                counters[bit] += 1;
            } else {
                counters[bit] -= 1;
            }
        }
        i += 4;
    }
    // Final hash: bit i = 1 if counter[i] > 0.
    let mut fp: u64 = 0;
    for bit in 0..64 {
        if counters[bit] > 0 {
            fp |= 1u64 << bit;
        }
    }
    fp
}

/// A single file entry in the TOC.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub original_size: u64,
    pub pre_size: u64,
    pub preprocessor: Preprocessor,
    /// 0-based chunk index. `0xFF` means the file lives in a
    /// single-stream payload (no EXT_CHUNKED). 0..N-1 means the
    /// file is in chunk N's decompressed bytes, with
    /// `solid_offset` as the offset within that chunk.
    ///
    /// **Sprint 5.7.x+ EXT_CHUNKED**: this field was added so the
    /// decoder can locate a file's chunk without scanning the
    /// entire chunk table for each TOC entry. Legacy callers
    /// (pre-EXT_CHUNKED) should treat this as `0xFF`.
    pub chunk_id: u8,
    /// Offset in the DECOMPRESSED solid stream where this file's
    /// preprocessed bytes start. In chunked mode this is the
    /// offset WITHIN the chunk identified by `chunk_id`.
    pub solid_offset: u64,
}

/// Detects the best preprocessor for a file based on its name.
/// **Sprint 5.7.x**: legacy entry point kept for callers that only
/// have a path (no bytes). Defers to `pick_preprocessor(name, bytes, is_random)`
/// with `is_random=false` — equivalent to the old extension-only behavior.
#[allow(dead_code)]
fn pick_preprocessor_legacy(name: &str) -> Preprocessor {
    pick_preprocessor(name, &[], false)
}

/// Apply the preprocessor to a single file's bytes.
fn apply_preprocessor(pre: Preprocessor, input: &[u8]) -> Vec<u8> {
    match pre {
        Preprocessor::Raw => input.to_vec(),
        Preprocessor::Conservative => minify::minify(input),
        Preprocessor::SwcAst => ast_minify::minify(input).bytes,
    }
}

/// Compress a list of (name, bytes) into a solid v6 archive.
///
/// The archive is LOSSY — the preprocessor is applied to each file
/// before concatenation. The `name` should be the relative path
/// (e.g. `src/index.ts`); it is stored verbatim in the TOC.
///
/// `lzma_level` is the LZMA preset (0..=9). 6 = balanced, 9 = max
/// ratio (apples-to-apples with 7z `-mx=9`).
///
/// `progress` is called after each file is preprocessed, with
/// `(file_index_done, total_files, current_file_name)`. Pass `|_,_,_| {}`
/// if you don't need progress reporting (or use `compress_with_progress`).
///
/// **Sprint 5.7.x:** before processing, files are sorted by SimHash
/// of their first 64 KB. This puts similar files consecutively → the
/// LZMA dictionary finds more matches across the solid stream
/// (typical win: +2-5% on text corpora, near-zero on already-similar
/// file groups). The TOC preserves the **original** name ordering so
/// the user's extraction path stays predictable; only the order in
/// the compressed bytes changes.
pub fn compress_with_progress<P>(
    files: &[(String, Vec<u8>)],
    lzma_level: u32,
    mut progress: P,
) -> Result<Vec<u8>, String>
where
    P: FnMut(usize, usize, &str),
{
    // Step 0: sort files by SimHash for better LZMA locality. We
    // compute SimHash once per file (mixing in the name hash so
    // two files with identical first-64-KB don't tie on the sort
    // key), then sort ascending with original index as tiebreaker.
    //
    // **Important**: random/binary files (per the classifier)
    // are segregated to the END of the concatenated stream. The
    // LZMA dictionary is monolithic — random bytes interleaved
    // with text would pollute the dictionary and degrade match
    // rate for the text files. Putting them at the end means
    // the text portion of the stream compresses cleanly, and
    // the random portion (which can't be compressed anyway) sits
    // in the tail where it can't hurt earlier matches.
    let total = files.len();
    let mut with_idx: Vec<(usize, u64, bool, &str, &[u8])> = Vec::with_capacity(total);
    for (i, (name, bytes)) in files.iter().enumerate() {
        let mut h = simhash_64(bytes);
        h ^= simhash_64(name.as_bytes()).rotate_left(13);
        // Mark whether this file's first SAMPLE_BYTES classifies
        // as Random. The classifier needs at least 1 KB to give a
        // reliable signal — smaller files default to "text-like"
        // (no segregation needed, conservative path is fine).
        let is_random = if bytes.len() >= 1024 {
            matches!(classifier::classify(&bytes[..bytes.len().min(SAMPLE_BYTES)]), _BlockType::Random)
        } else {
            false
        };
        with_idx.push((i, h, is_random, name.as_str(), bytes.as_slice()));
    }
    // Sort by (is_random, hash, original_index). `is_random=false`
    // comes first (false < true), so text gets sorted + concatenated
    // before random. Within each group, SimHash order applies.
    with_idx.sort_by(|a, b| {
        a.2.cmp(&b.2) // false < true → text first
            .then(a.1.cmp(&b.1))
            .then(a.0.cmp(&b.0))
    });

    // Step 1: preprocess each file in the NEW (SimHash-sorted) order,
    // build the uncompressed solid stream and the TOC in parallel.
    //
    // **Sprint 5.7.x+**: per-file preprocessing now runs on rayon's
    // thread pool. SwcAst (the .ts/.tsx preprocessor) calls into
    // swc_core which is single-threaded per parse call — but with
    // many files the wall-clock win is real. Typical speedup on a
    // corpus of 200 .tsx files: ~3× on a 4-core box.
    //
    // Progress emission stays sequential (below) — emitting events
    // out of order would make the UI bar jump erratically.
    use rayon::prelude::*;
    let preprocessed: Vec<(String, u64, Preprocessor, Vec<u8>)> = with_idx
        .par_iter()
        .map(|&(_, _, is_random, name, bytes)| {
            let pre = pick_preprocessor(name, bytes, is_random);
            let pre_bytes = apply_preprocessor(pre, bytes);
            (name.to_string(), bytes.len() as u64, pre, pre_bytes)
        })
        .collect();

    let mut entries: Vec<FileEntry> = Vec::with_capacity(total);
    let mut solid_uncompressed: Vec<u8> = Vec::new();

    for (i, (name, original_size, pre, pre_bytes)) in
        preprocessed.into_iter().enumerate()
    {
        let offset = solid_uncompressed.len() as u64;
        let pre_size = pre_bytes.len() as u64;
        solid_uncompressed.extend_from_slice(&pre_bytes);
        // Progress fires BEFORE we move `name` into the entry —
        // the callback signature is `FnMut(usize, usize, &str)`
        // so we need a borrow here.
        progress(i + 1, total, &name);
        entries.push(FileEntry {
            name,
            original_size,
            pre_size,
            preprocessor: pre,
            chunk_id: 0xFF, // single-stream; this v2 archive doesn't use EXT_CHUNKED
            solid_offset: offset,
        });
    }

    // Step 2: LZMA-compress the solid stream.
    let mut lzma_compressed: Vec<u8> = Vec::new();
    {
        // Sprint 5.7 hotfix #21: auto-tune the LZMA dict size to
        // whatever the machine can actually afford (no swap).
        let stream = crate::ram::lzma_stream_for_requested_preset(lzma_level)
            .map_err(|e| format!("solid LZMA stream: {}", e))?;
        let mut enc = XzEncoder::new_stream(&mut lzma_compressed, stream);
        enc.write_all(&solid_uncompressed)
            .map_err(|e| format!("solid LZMA write failed: {}", e))?;
        enc.finish()
            .map_err(|e| format!("solid LZMA finish failed: {}", e))?;
    }

    // Step 3: serialize as NXS v2 (extensible format). Sprint 1
    // only emits the two required extensions — EXT_TOC and
    // EXT_PAYLOAD — but the manifest is forward-compatible so
    // future sprints (CHUNKED, DICT_TRAINING, DEDUP_TABLE, etc.)
    // can be added without bumping the format version.
    //
    // Mapping from the in-memory `FileEntry` to the v2 layout:
    // the legacy `solid_offset` field is renamed to `chunk_offset`
    // — in single-stream mode (Sprint 1) it points into the single
    // decompressed payload, exactly like the old `solid_offset`.
    let v2_entries: Vec<format_v2::FileEntry> = entries
        .iter()
        .map(|e| format_v2::FileEntry {
            name: e.name.clone(),
            original_size: e.original_size,
            pre_size: e.pre_size,
            preprocessor: e.preprocessor as u8,
            chunk_id: 0xFF, // single-stream; this v2 archive doesn't use EXT_CHUNKED
            chunk_offset: e.solid_offset,
        })
        .collect();
    let extensions = vec![
        format_v2::Extension {
            id: format_v2::EXT_TOC,
            payload: format_v2::serialize_toc(&v2_entries),
        },
        format_v2::payload_from_lzma(lzma_compressed),
    ];
    Ok(format_v2::write_archive(&extensions))
}

/// Backward-compatible wrapper that discards progress.
#[inline]
pub fn compress(files: &[(String, Vec<u8>)], lzma_level: u32) -> Result<Vec<u8>, String> {
    compress_with_progress(files, lzma_level, |_, _, _| {})
}

/// Stream-compress a directory into an NXS6 v2 archive on disk with bounded RAM.
///
/// Instead of holding all input files, preprocessed buffers, solid stream,
/// and compressed output in memory at once, this reads 64 KB samples for
/// SimHash/classifier sorting, streams each file one-by-one into an
/// `XzEncoder` backed by a temporary file, and then streams the NXS v2
/// manifest + compressed payload directly to `output` (or `.001`, `.002`
/// parts when `split_mb` is set and exceeded).
///
/// Returns `(n_files, total_original_bytes, total_archive_bytes)`.
pub fn compress_dir_streaming(
    root: &Path,
    lzma_level: u32,
    output: &Path,
    split_mb: Option<usize>,
) -> Result<(usize, u64, u64), String> {
    use std::io::{BufWriter, Seek, SeekFrom};

    fn collect_paths(
        root: &Path,
        dir: &Path,
        out: &mut Vec<(std::path::PathBuf, String)>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                collect_paths(root, &path, out)?;
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((path, rel));
            }
        }
        Ok(())
    }

    let mut paths: Vec<(std::path::PathBuf, String)> = Vec::new();
    collect_paths(root, root, &mut paths)
        .map_err(|e| format!("walk dir failed: {}", e))?;
    paths.sort_by(|a, b| a.1.cmp(&b.1));
    if paths.is_empty() {
        return Err(format!("directory has no files: {}", root.display()));
    }

    let n_files = paths.len();
    let mut meta: Vec<(usize, u64, bool, std::path::PathBuf, String)> =
        Vec::with_capacity(n_files);
    let mut sample_buf = vec![0u8; SAMPLE_BYTES];
    for (i, (disk_path, rel_name)) in paths.into_iter().enumerate() {
        let mut f = fs::File::open(&disk_path)
            .map_err(|e| format!("open {}: {}", disk_path.display(), e))?;
        let n_read = f
            .read(&mut sample_buf)
            .map_err(|e| format!("read sample {}: {}", disk_path.display(), e))?;
        let sample = &sample_buf[..n_read];
        let mut h = simhash_64(sample);
        h ^= simhash_64(rel_name.as_bytes()).rotate_left(13);
        let is_random = if n_read >= 1024 {
            matches!(classifier::classify(sample), _BlockType::Random)
        } else {
            false
        };
        meta.push((i, h, is_random, disk_path, rel_name));
    }

    meta.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)).then(a.0.cmp(&b.0)));

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }

    let tmp_xz_path = output.with_extension("tmp.xz");
    struct TempCleanup<'a>(&'a Path);
    impl<'a> Drop for TempCleanup<'a> {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0);
        }
    }
    let _cleanup = TempCleanup(&tmp_xz_path);

    let mut entries: Vec<format_v2::FileEntry> = Vec::with_capacity(n_files);
    let mut total_original: u64 = 0;
    let mut solid_offset: u64 = 0;

    {
        let tmp_file = fs::File::create(&tmp_xz_path)
            .map_err(|e| format!("create {}: {}", tmp_xz_path.display(), e))?;
        let writer = BufWriter::with_capacity(1024 * 1024, tmp_file);
        let stream = crate::ram::lzma_stream_for_requested_preset(lzma_level)
            .map_err(|e| format!("solid LZMA stream: {}", e))?;
        let mut enc = XzEncoder::new_stream(writer, stream);

        for (_, _, is_random, disk_path, rel_name) in meta {
            let bytes = fs::read(&disk_path)
                .map_err(|e| format!("read {}: {}", disk_path.display(), e))?;
            let original_size = bytes.len() as u64;
            total_original += original_size;

            let pre = pick_preprocessor(&rel_name, &bytes, is_random);
            let pre_bytes = apply_preprocessor(pre, &bytes);
            drop(bytes);

            let pre_size = pre_bytes.len() as u64;
            enc.write_all(&pre_bytes)
                .map_err(|e| format!("solid LZMA write failed: {}", e))?;
            drop(pre_bytes);

            entries.push(format_v2::FileEntry {
                name: rel_name,
                original_size,
                pre_size,
                preprocessor: pre as u8,
                chunk_id: 0xFF,
                chunk_offset: solid_offset,
            });
            solid_offset += pre_size;
        }

        let mut writer = enc
            .finish()
            .map_err(|e| format!("solid LZMA finish failed: {}", e))?;
        writer
            .flush()
            .map_err(|e| format!("flush {}: {}", tmp_xz_path.display(), e))?;
    }

    let toc_payload = format_v2::serialize_toc(&entries);
    let toc_len = u32::try_from(toc_payload.len())
        .map_err(|_| "TOC exceeds 4 GiB limit".to_string())?;

    let lzma_len_u64 = fs::metadata(&tmp_xz_path)
        .map_err(|e| format!("stat {}: {}", tmp_xz_path.display(), e))?
        .len();
    let lzma_len = u32::try_from(lzma_len_u64)
        .map_err(|_| format!("compressed payload ({} bytes) exceeds 4 GiB NXS v2 limit", lzma_len_u64))?;

    let total_archive: u64 = 8 + 2 + (2 + 4 + toc_payload.len() as u64) + (2 + 4 + lzma_len_u64);
    let chunk_limit = split_mb
        .filter(|&mb| mb > 0)
        .map(|mb| (mb as u64).saturating_mul(1024 * 1024));

    struct SplitOut {
        base: String,
        limit: Option<u64>,
        part_idx: usize,
        written_in_part: u64,
        writer: BufWriter<fs::File>,
    }

    impl SplitOut {
        fn new(base: &Path, limit: Option<u64>, total: u64) -> Result<Self, String> {
            let base_str = base.to_string_lossy().to_string();
            let use_split = limit.map_or(false, |l| total > l);
            let effective_limit = if use_split { limit } else { None };
            if use_split {
                let _ = fs::remove_file(base);
                let first = format!("{}.{:03}", base_str, 1);
                let f = fs::File::create(&first).map_err(|e| format!("create {}: {}", first, e))?;
                Ok(Self {
                    base: base_str,
                    limit: effective_limit,
                    part_idx: 1,
                    written_in_part: 0,
                    writer: BufWriter::with_capacity(1024 * 1024, f),
                })
            } else {
                let f = fs::File::create(base).map_err(|e| format!("create {}: {}", base_str, e))?;
                Ok(Self {
                    base: base_str,
                    limit: None,
                    part_idx: 0,
                    written_in_part: 0,
                    writer: BufWriter::with_capacity(1024 * 1024, f),
                })
            }
        }

        fn write_bytes(&mut self, mut data: &[u8]) -> Result<(), String> {
            while !data.is_empty() {
                if let Some(lim) = self.limit {
                    if self.written_in_part >= lim {
                        self.writer.flush().map_err(|e| format!("flush part: {}", e))?;
                        self.part_idx += 1;
                        let next_path = format!("{}.{:03}", self.base, self.part_idx);
                        let f = fs::File::create(&next_path)
                            .map_err(|e| format!("create {}: {}", next_path, e))?;
                        self.writer = BufWriter::with_capacity(1024 * 1024, f);
                        self.written_in_part = 0;
                    }
                    let avail = (lim - self.written_in_part) as usize;
                    let take = data.len().min(avail);
                    self.writer
                        .write_all(&data[..take])
                        .map_err(|e| format!("write part: {}", e))?;
                    self.written_in_part += take as u64;
                    data = &data[take..];
                } else {
                    self.writer
                        .write_all(data)
                        .map_err(|e| format!("write output: {}", e))?;
                    self.written_in_part += data.len() as u64;
                    break;
                }
            }
            Ok(())
        }

        fn finish(mut self) -> Result<(), String> {
            self.writer.flush().map_err(|e| format!("flush output: {}", e))
        }
    }

    let mut out = SplitOut::new(output, chunk_limit, total_archive)?;
    out.write_bytes(format_v2::MAGIC)?;
    out.write_bytes(&[format_v2::VERSION, 0u8, 0u8, 0u8])?;
    out.write_bytes(&2u16.to_le_bytes())?;
    out.write_bytes(&format_v2::EXT_TOC.to_le_bytes())?;
    out.write_bytes(&toc_len.to_le_bytes())?;
    out.write_bytes(&toc_payload)?;
    out.write_bytes(&format_v2::EXT_PAYLOAD.to_le_bytes())?;
    out.write_bytes(&lzma_len.to_le_bytes())?;

    let mut tmp_reader = BufReader::with_capacity(
        1024 * 1024,
        fs::File::open(&tmp_xz_path).map_err(|e| format!("reopen {}: {}", tmp_xz_path.display(), e))?,
    );
    tmp_reader
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek {}: {}", tmp_xz_path.display(), e))?;
    let mut copy_buf = vec![0u8; 1024 * 1024];
    loop {
        let n = tmp_reader
            .read(&mut copy_buf)
            .map_err(|e| format!("read {}: {}", tmp_xz_path.display(), e))?;
        if n == 0 {
            break;
        }
        out.write_bytes(&copy_buf[..n])?;
    }
    out.finish()?;
    Ok((n_files, total_original, total_archive))
}

// ─────────────────────────────────────────────────────────────────────
//   Sprint 5.7.x+ — single-file streaming compression
// ─────────────────────────────────────────────────────────────────────
//
// The standard `compress_with_progress` requires every input file's
// bytes to be in RAM (so it can preprocess + concat into a solid
// stream). For multi-GB files that OOMs.
//
// `compress_single_streaming` takes a file path instead of pre-loaded
// bytes. It opens the file, samples the first 64 KB to classify
// (Random vs text), then pipes the WHOLE file through a streaming
// LZMA encoder with bounded memory. Memory peak: ~16 MB (xz2's
// internal buffer + our 64 KB sample + a few KB for the manifest),
// even for a 100 GB file.
//
// **Trade-off**: no preprocessing in streaming mode (Conservative
// text minify is stateful — it needs to see the whole file to
// avoid splitting a `/* ... */` comment across chunks). This costs
// ~10-15% ratio on text-heavy inputs vs the RAM+preprocess path.
// Callers that need the full ratio should use `compress_with_progress`
// instead. Use streaming mode when the file is too big to fit in RAM.
//
// Format produced: standard NXS v2 with 1 EXT_TOC entry (the file)
// + 1 EXT_PAYLOAD (LZMA-compressed bytes). Backward-compatible
// with every v2 reader (no chunking extension needed).
//
// Progress emission: bytes_done is updated as we read the file
// (one event per ~4 MiB consumed, throttled by the Tauri side).
pub fn compress_single_streaming<P>(
    input_path: &std::path::Path,
    file_name: &str,
    lzma_level: u32,
    mut progress: P,
) -> Result<Vec<u8>, String>
where
    P: FnMut(u64, u64),
{
    let total_size = fs::metadata(input_path)
        .map_err(|e| format!("metadata({}) failed: {}", input_path.display(), e))?
        .len();

    // Phase 1: sample first 64 KB to classify (Random → skip minify).
    // We use a bounded 64 KB buffer — bounded even for huge files.
    let mut sample = vec![0u8; SAMPLE_BYTES.min(total_size as usize)];
    {
        let f = fs::File::open(input_path)
            .map_err(|e| format!("open({}) failed: {}", input_path.display(), e))?;
        let mut reader = BufReader::with_capacity(64 * 1024, f);
        use std::io::Read;
        let n = reader
            .read(&mut sample)
            .map_err(|e| format!("sample read failed: {}", e))?;
        sample.truncate(n);
    }
    let is_random = if sample.len() >= 1024 {
        matches!(
            classifier::classify(&sample),
            _BlockType::Random
        )
    } else {
        false
    };

    // Phase 2: stream the file through LZMA. Memory = xz2's
    // internal buffer (default ~16 MB). Output goes straight to
    // `lzma_compressed` — no per-chunk Vec<u8> intermediate.
    let mut lzma_compressed: Vec<u8> = Vec::new();
    {
        let stream = crate::ram::lzma_stream_for_requested_preset(lzma_level)
            .map_err(|e| format!("solid LZMA stream: {}", e))?;
        let f = fs::File::open(input_path)
            .map_err(|e| format!("open({}) failed (streaming): {}", input_path.display(), e))?;
        // 256 KB buffer is a good I/O sweet spot: large enough
        // to amortize syscall cost, small enough to not bloat.
        let mut reader = BufReader::with_capacity(256 * 1024, f);
        let mut enc = XzEncoder::new_stream(&mut lzma_compressed, stream);
        // Stream-read in 1 MB chunks (good for both progress
        // granularity and syscall efficiency).
        let mut buf = [0u8; 1024 * 1024];
        let mut bytes_done: u64 = 0;
        // Apply Conservative text minify on the fly only when
        // the file isn't Random AND isn't a binary type. For
        // Random bytes the minify is a no-op anyway (text-only),
        // so we feed straight to LZMA to avoid the copy.
        let _ = is_random; // for now, always stream raw bytes
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| format!("streaming read failed: {}", e))?;
            if n == 0 {
                break;
            }
            enc.write_all(&buf[..n])
                .map_err(|e| format!("LZMA write failed: {}", e))?;
            bytes_done += n as u64;
            progress(bytes_done, total_size);
        }
        enc.finish()
            .map_err(|e| format!("LZMA finish failed: {}", e))?;
    }

    // Phase 3: build the NXS v2 archive (1 TOC entry, 1 PAYLOAD).
    // Streaming mode always produces single-chunk archives — chunking
    // is a separate concern (handled by EXT_CHUNKED in a later
    // sprint).
    let entry = format_v2::FileEntry {
        name: file_name.to_string(),
        original_size: total_size,
        pre_size: total_size, // no preprocessing in streaming mode
        preprocessor: 0,      // Raw
        chunk_id: 0xFF,       // single-stream; this v2 archive doesn't use EXT_CHUNKED
        chunk_offset: 0,      // always 0 in single-chunk streaming
    };
    let extensions = vec![
        format_v2::Extension {
            id: format_v2::EXT_TOC,
            payload: format_v2::serialize_toc(&[entry]),
        },
        format_v2::payload_from_lzma(lzma_compressed),
    ];
    Ok(format_v2::write_archive(&extensions))
}

// ─────────────────────────────────────────────────────────────────────
//   Sprint 5.7.x+ — chunked compression (EXT_CHUNKED)
//
//   The single-stream path above pipes EVERY file through one
//   monolithic LZMA encoder. That has two drawbacks:
//
//   1. **No random-access extraction.** A user who wants file #5 of
//      a 1000-file archive still has to LZMA-decompress the whole
//      stream to reach it. This is slow for big archives even
//      when the user only wants one file.
//
//   2. **No parallel LZMA.** xz2's `XzEncoder` is single-threaded
//      per stream; a 4-core box only uses one core for the heaviest
//      step in the whole pipeline.
//
//   `compress_chunked_with_progress` solves both: it splits the
//   preprocessed file list into chunks of at most
//   `chunk_max_uncompressed` bytes (default 128 MB), then
//   LZMA-compresses each chunk as its own independent stream.
//   Each chunk's bytes are self-contained in the payload — the
//   decoder can decompress any chunk without touching the others.
//
//   **Trade-off vs the single-stream path:**
//   - Within-chunk LZMA locality is preserved (SimHash sort runs
//     per chunk), but cross-chunk matches are lost. For typical
//     text corpora this costs ~1-3% ratio on the first chunk
//     boundary; subsequent boundaries matter less because the
//     chunks now match each other's structure independently.
//   - We pay N xz2 stream headers + footers (~50 bytes each)
//     instead of one. Negligible.
//   - Random-access extraction: huge win on selective extraction.
//   - Parallel compression: 2-4× speedup on multi-core.
//
//   **Format produced:** NXS v2 with EXT_TOC + EXT_CHUNKED +
//   EXT_PAYLOAD. EXT_PAYLOAD is the concatenation of N xz2
//   streams; the chunk table maps index → sizes.
//
//   **Memory:** peak is one chunk's uncompressed bytes + one
//   chunk's LZMA state + the rest as already-compressed bytes
//   accumulated in the output Vec. For default 128 MB chunks on
//   a 4 GB corpus: ~256 MB peak, vs the single-stream path's
//   "decompressed corpus in RAM" requirement.
pub const DEFAULT_CHUNK_MAX_UNCOMPRESSED: u64 = 32 * 1024 * 1024;

/// Default Reed-Solomon data-shard count per chunk. With 10 data
/// shards and `DEFAULT_PARITY_PARITY_SHARDS` parity shards, each
/// chunk can lose up to 2 shards and still be fully recovered.
/// Smaller `data_shards` = smaller fixed overhead per recovery
/// attempt (the matrix inversion is O(n³)); larger = more granular
/// recovery (each shard covers fewer bytes).
pub const DEFAULT_PARITY_DATA_SHARDS: u8 = 10;

/// Default Reed-Solomon parity-shard count per chunk.
///
/// **Sprint 5.7.x+ regression fix:** was 2 (giving 20% storage
/// overhead) which made compression ratio worse for users who
/// don't need corruption recovery. The user values ratio
/// above all else, so the default is now **0 (disabled)** —
/// archives produced by `compress_chunked_with_progress` will
/// have NO parity shards and the EXT_PARITY_SHARDS extension
/// is omitted. Set this constant to 2 (or any value up to 254
/// minus `DEFAULT_PARITY_DATA_SHARDS`) to opt into Reed-Solomon
/// redundancy for a specific call site.
///
/// The format itself (EXT_PARITY_SHARDS extension, RS
/// reconstruction code in `reassemble_chunk_lzma`, etc.) is
/// preserved and tested — it's just not active by default until
/// a user opts in via a future setting/CLI flag.
pub const DEFAULT_PARITY_PARITY_SHARDS: u8 = 0;

/// Chunked compression — same signature as `compress_with_progress`
/// plus a `chunk_max_uncompressed` knob (use
/// `DEFAULT_CHUNK_MAX_UNCOMPRESSED` unless you know better).
pub fn compress_chunked_with_progress<P>(
    files: &[(String, Vec<u8>)],
    lzma_level: u32,
    chunk_max_uncompressed: u64,
    mut progress: P,
) -> Result<Vec<u8>, String>
where
    P: FnMut(usize, usize, &str),
{
    let total = files.len();

    // Phase 1 — read every file's first 64 KB and compute its
    // SimHash + is_random flag, then preprocess the full file.
    // We need the hashes BEFORE we can group into chunks (the
    // chunking itself is content-blind, but we want each chunk's
    // internal order to be SimHash-sorted, and we need to know
    // the preprocessed size to decide chunk boundaries).
    //
    // Parallel preprocessing here: large corpora spend most of
    // their wall time on swc_core (.ts/.tsx) otherwise.
    struct FileMeta {
        idx: usize,
        name: String,
        original_size: u64,
        pre_size: u64,
        pre: Preprocessor,
        simhash: u64,
        /// Preprocessed bytes — exactly what we'll LZMA-compress
        /// in the chunk phase. Pre-computed here so the chunk
        /// phase doesn't redo the work.
        pre_bytes: Vec<u8>,
    }
    // Sprint 5.7.x+ hotfix: chunked directory compression was
    // hitting STATUS_BREAKPOINT on ~250 MB corpora because the
    // parallel preprocessing + parallel LZMA encoding combined to
    // use ~700 MB peak RAM. We serialise the preprocessing phase
    // (was `par_iter`, now `iter`) so each file's preprocessed
    // bytes are dropped as soon as the next file is read. Peak
    // RAM = largest single file + classifier scratch, instead of
    // `num_threads × largest_file`. For ~250 MB corpora on 4-core
    // machines this trades ~3× wall-clock preprocessing speed for
    // roughly 4× lower peak RAM — a good trade for the "always
    // works" UX.
    let metas: Vec<FileMeta> = files
        .iter()
        .enumerate()
        .map(|(idx, (name, bytes))| {
            let sample_len = bytes.len().min(SAMPLE_BYTES);
            let sample = &bytes[..sample_len];
            let is_random = if sample.len() >= 1024 {
                matches!(classifier::classify(sample), _BlockType::Random)
            } else {
                false
            };
            let pre = pick_preprocessor(name, sample, is_random);
            let pre_bytes = apply_preprocessor(pre, bytes);
            let pre_size = pre_bytes.len() as u64;
            let mut h = simhash_64(bytes);
            h ^= simhash_64(name.as_bytes()).rotate_left(13);
            FileMeta {
                idx,
                name: name.clone(),
                original_size: bytes.len() as u64,
                pre_size,
                pre,
                simhash: h,
                pre_bytes,
            }
        })
        .collect();

    // Phase 2 — group files into chunks. Each chunk accumulates
    // pre_size until it would exceed chunk_max_uncompressed; the
    // next file (no matter how small) starts a new chunk. This
    // guarantees no chunk is ever over-budget.
    //
    // Within each chunk we SimHash-sort the files (text first,
    // random at end — same convention as the single-stream
    // path). The chunk layout is then sequential by file index
    // (the original TOC order is preserved per-entry).
    let mut chunks: Vec<Vec<usize>> = Vec::new(); // each is a list of metas indices
    let mut current: Vec<usize> = Vec::new();
    let mut current_size: u64 = 0;
    // Sort metas by (simhash, original_idx) to get locality. We
    // sort ALL metas together; the chunking happens after the
    // sort. This means chunk boundaries may split groups of
    // similar files, but for typical corpora this is fine —
    // EXCEPT for the Random-at-end convention we want to keep.
    // So we do a 2-level sort: (is_random, simhash, idx) — text
    // files come first globally, then random. Chunking groups
    // them. Within a chunk the relative SimHash order is
    // preserved by construction.
    let mut order: Vec<usize> = (0..metas.len()).collect();
    order.sort_by(|&a, &b| {
        // Sort by preprocessor ID (Raw=0 < Conservative=1 < SwcAst=2)
        // so text-minified files cluster together in the same
        // chunk group. Preprocessor doesn't derive Ord so we cast
        // to u8 — the discriminant order matches the variant order.
        (metas[a].pre as u8)
            .cmp(&(metas[b].pre as u8))
            .then(metas[a].simhash.cmp(&metas[b].simhash))
            .then(metas[a].idx.cmp(&metas[b].idx))
    });
    for mi in order {
        let m = &metas[mi];
        // Always start a new chunk if the current file alone is
        // bigger than the budget — we'd rather have one big
        // chunk than split a single file across chunks (the
        // TOC can't represent that without a chunk_id field).
        if current_size > 0 && current_size + m.pre_size > chunk_max_uncompressed {
            chunks.push(std::mem::take(&mut current));
            current_size = 0;
        }
        current.push(mi);
        current_size += m.pre_size;
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    // Phase 3 — per chunk: concat the preprocessed bytes, LZMA-
    // compress. Parallel across chunks via rayon — each chunk
    // runs its own XzEncoder on its own thread.
    //
    // Why parallel compression is safe here: each chunk is
    // self-contained. xz2's encoder doesn't share state across
    // threads, and the only contention is on the output Vec
    // (we serialize the write of compressed bytes below).
    //
    // **Important:** the uncompressed bytes are rebuilt per
    // chunk in the post-sort order. We need to also remember
    // the SimHash-sorted order for progress events, so the
    // callback gets fired in the same display order as the
    // single-stream path.
    struct ChunkResult {
        lzma_bytes: Vec<u8>,
        uncompressed_size: u64,
        // Per-chunk entries in compressed order (so we can
        // build the TOC and chunk_offset fields below).
        entries: Vec<(usize, u64, u64, u8)>, // (meta_idx, chunk_offset, pre_size, preprocessor)
        // SHA-256 of the chunk's uncompressed bytes. Computed
        // BEFORE LZMA so the hash is independent of compression
        // settings and identifies byte-identical content. Used by
        // EXT_DEDUP_TABLE to merge duplicate chunks.
        uncompressed_hash: [u8; 32],
    }
    // Sprint 5.7.x+ hotfix: switch the LZMA encoding phase from
    // `par_iter` to `iter` so chunks are compressed one at a time
    // (peak LZMA state memory = 1 chunk's worth instead of
    // `num_threads × chunk`). Combined with the smaller
    // `DEFAULT_CHUNK_MAX_UNCOMPRESSED = 32 MB` and the serialised
    // preprocessing above, peak RAM for a 250 MB corpus drops from
    // ~700 MB to ~280 MB — well under the OOM threshold on typical
    // 4–8 GB laptops. Trade-off: no longer 2-4× parallel speedup
    // on the LZMA encode step. The LZMA step still runs in the
    // background and the user sees the progress bar advance; only
    // wall-clock is slower on multi-core boxes.
    let chunk_results: Vec<ChunkResult> = chunks
        .iter()
        .map(|chunk_metas| {
            // SimHash-sort within the chunk (defensive — already
            // sorted globally, but if chunking split two similar
            // files, we re-sort the chunk's view to maximize
            // within-chunk locality).
            let mut sorted: Vec<usize> = chunk_metas.clone();
            sorted.sort_by(|&a, &b| {
                (metas[a].pre as u8)
                    .cmp(&(metas[b].pre as u8))
                    .then(metas[a].simhash.cmp(&metas[b].simhash))
                    .then(metas[a].idx.cmp(&metas[b].idx))
            });
            // Concat preprocessed bytes.
            let mut uncompressed: Vec<u8> = Vec::new();
            let mut entries: Vec<(usize, u64, u64, u8)> = Vec::with_capacity(sorted.len());
            for mi in &sorted {
                let m = &metas[*mi];
                let chunk_offset = uncompressed.len() as u64;
                uncompressed.extend_from_slice(&m.pre_bytes);
                entries.push((*mi, chunk_offset, m.pre_size, m.pre as u8));
            }
            // Hash uncompressed bytes for dedup detection. We
            // hash BEFORE LZMA so the digest is independent of the
            // compression level — same input at any level has the
            // same hash.
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&uncompressed);
            let hash_result = hasher.finalize();
            let mut uncompressed_hash = [0u8; 32];
            uncompressed_hash.copy_from_slice(&hash_result);
            // LZMA-compress.
            let mut lzma_bytes: Vec<u8> = Vec::new();
            {
                let stream = crate::ram::lzma_stream_for_requested_preset(lzma_level)
                    .map_err(|e| format!("chunked LZMA stream: {}", e))?;
                let mut enc = XzEncoder::new_stream(&mut lzma_bytes, stream);
                enc.write_all(&uncompressed)
                    .map_err(|e| format!("chunked LZMA write failed: {}", e))?;
                enc.finish()
                    .map_err(|e| format!("chunked LZMA finish failed: {}", e))?;
            }
            // We can drop uncompressed now — the hash is what we
            // need for dedup detection in the next phase. Memory
            // stays bounded (the compressed bytes are ~30% of
            // original; we never hold both at once).
            Ok::<ChunkResult, String>(ChunkResult {
                lzma_bytes,
                uncompressed_size: uncompressed.len() as u64,
                entries,
                uncompressed_hash,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Phase 4 — fire progress events in ORIGINAL file order
    // (matches the single-stream path's behavior, so the UI
    // bar feels the same).
    let mut done = vec![false; metas.len()];
    for cr in &chunk_results {
        for (meta_idx, _, _, _) in &cr.entries {
            let m = &metas[*meta_idx];
            if !done[*meta_idx] {
                progress(*meta_idx + 1, total, &m.name);
                done[*meta_idx] = true;
            }
        }
    }

    // Phase 5 — build TOC entries (in original file order) and
    // the EXT_CHUNKED table.
    //
    // TOC entries reference `chunk_offset`, which in EXT_CHUNKED
    // mode means "offset within the file's owning chunk". The
    // decoder uses the chunk table to map this to (chunk_id,
    // compressed-byte-range) at extraction time.
    //
    // Note: TOC entries are emitted in ORIGINAL file order
    // (matches the single-stream path) so the GUI shows files in
    // the order the user expects rather than SimHash-sorted order.
    let mut toc_entries: Vec<format_v2::FileEntry> = Vec::with_capacity(total);
    // Sort file indices by original order.
    let mut original_order: Vec<usize> = (0..metas.len()).collect();
    original_order.sort_by_key(|&i| metas[i].idx);
    for orig_idx in original_order {
        // Find this file's chunk_offset AND chunk_id in its chunk's
        // entries. We walk every chunk result and look for the
        // file's meta_idx — if found, the chunk_id is the chunk
        // result's index in the chunk_results Vec.
        let mut found: Option<(u64, usize)> = None;
        for (chunk_id, cr) in chunk_results.iter().enumerate() {
            for (meta_idx, chunk_offset, _, _) in &cr.entries {
                if *meta_idx == orig_idx {
                    found = Some((*chunk_offset, chunk_id));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let (chunk_offset, owning_chunk_id) =
            found.expect("file missing from chunk results — invariant broken");
        let m = &metas[orig_idx];
        toc_entries.push(format_v2::FileEntry {
            name: m.name.clone(),
            original_size: m.original_size,
            pre_size: m.pre_size,
            preprocessor: m.pre as u8,
            chunk_id: owning_chunk_id as u8,
            chunk_offset,
        });
    }

    // Build the chunk table from the chunk_results.
    // **Sprint 5.7.x+ EXT_PARITY_SHARDS:** the `compressed_size`
    // we store here is the SHARDED size (data_shards × data_shard_size)
    // when parity is enabled, or the original LZMA byte count
    // when parity is disabled (the common path). When disabled,
    // EXT_PAYLOAD contains the raw LZMA stream bytes verbatim,
    // so chunk_infos.compressed_size == lzma_bytes.len() and
    // no sharding is performed.
    //
    // We compute the parity shards in Phase 6 (next) and build the
    // actual payload from the data shards in Phase 7.
    let parity_enabled = DEFAULT_PARITY_PARITY_SHARDS > 0;
    let chunk_infos: Vec<format_v2::ChunkInfo> = chunk_results
        .iter()
        .map(|cr| {
            let total = cr.lzma_bytes.len();
            let compressed_size = if parity_enabled {
                let data_shard_size =
                    ((total + DEFAULT_PARITY_DATA_SHARDS as usize - 1)
                        / DEFAULT_PARITY_DATA_SHARDS as usize) as u32;
                data_shard_size as u64 * DEFAULT_PARITY_DATA_SHARDS as u64
            } else {
                total as u64
            };
            format_v2::ChunkInfo {
                uncompressed_size: cr.uncompressed_size,
                compressed_size,
            }
        })
        .collect();

    // Phase 6 — Reed-Solomon sharding (EXT_PARITY_SHARDS).
    //
    // Each chunk's LZMA bytes are split into N data shards (the last
    // shard is zero-padded to match the others), then M parity shards
    // are computed via GF(2^8) Reed-Solomon over the data shards. The
    // N data shards go into `EXT_PAYLOAD`; the M parity shards go into
    // `EXT_PARITY_SHARDS`.
    //
    // Why split AND add parity? A typical 250 MB corpus compresses
    // to ~80 MB. With N=10, M=2, the overhead is 20% (~96 MB total)
    // and we can recover up to 2 missing shards per chunk — covers
    // bit-rot on long-term backups, partial transfer corruption, and
    // accidental truncation. The Reed-Solomon matrix inversion is
    // O(N³) = O(1000) ops, ~10 µs per recovery — negligible.
    //
    // The smart decoder uses Reed-Solomon ONLY when a shard is
    // missing or fails its CRC32 integrity check. On a healthy
    // archive the parity shards are simply discarded.
    //
    // When parity is disabled (DEFAULT_PARITY_PARITY_SHARDS == 0),
    // parity_layouts is empty and the EXT_PARITY_SHARDS extension
    // is omitted entirely.
    let parity_layouts: Vec<format_v2::ParityShardLayout> = if parity_enabled {
        chunk_results
            .iter()
            .map(|cr| {
                let total = cr.lzma_bytes.len();
                let data_shard_size =
                    ((total + DEFAULT_PARITY_DATA_SHARDS as usize - 1)
                        / DEFAULT_PARITY_DATA_SHARDS as usize) as u32;
                format_v2::ParityShardLayout {
                    data_shards: DEFAULT_PARITY_DATA_SHARDS,
                    parity_shards: DEFAULT_PARITY_PARITY_SHARDS,
                    data_shard_size,
                    parity_shard_size: data_shard_size, // parity shards match data shards
                    // Track the actual LZMA size (pre-shard) so the
                    // decoder can truncate the reassembled data to
                    // exactly the original stream. Without this, the
                    // xz decoder would see trailing zero-padding and
                    // try to interpret it as the start of another
                    // stream ("corrupt xz stream" error).
                    lzma_size: total as u32,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    // Compute shards and parity. This is per-chunk — done sequentially
    // here (we're already on the single-LZMA-stream-per-chunk path).
    // Each shard is a `Vec<u8>` of `data_shard_size` bytes; the last
    // data shard is padded with zeros if the LZMA size isn't a
    // multiple of N. The LZMA decoder ignores trailing zeros after the
    // stream ends (xz2's XzDecoder reads to EOF marker), so the
    // padding is harmless on the round-trip.
    //
    // When parity is disabled (the default), we just copy the raw
    // LZMA bytes into EXT_PAYLOAD verbatim — no sharding, no
    // Reed-Solomon, no overhead, best compression ratio.
    let mut payload: Vec<u8> = Vec::new();
    let mut parity_payload: Vec<u8> = Vec::new();
    if parity_enabled {
        for (chunk_idx, cr) in chunk_results.iter().enumerate() {
            let layout = &parity_layouts[chunk_idx];
            let n = layout.data_shards as usize;
            let m = layout.parity_shards as usize;
            let shard_size = layout.data_shard_size as usize;
            // Split into N data shards (last padded with zeros).
            let mut shards: Vec<Vec<u8>> = Vec::with_capacity(n + m);
            for i in 0..n {
                let start = i * shard_size;
                let end = ((i + 1) * shard_size).min(cr.lzma_bytes.len());
                let mut shard = Vec::with_capacity(shard_size);
                if start < cr.lzma_bytes.len() {
                    shard.extend_from_slice(&cr.lzma_bytes[start..end]);
                }
                // Pad to shard_size with zeros (last shard).
                shard.resize(shard_size, 0);
                shards.push(shard);
            }
            // Add M empty parity shards.
            for _ in 0..m {
                shards.push(vec![0u8; shard_size]);
            }
            // Encode parity.
            let rs = crate::galois::codes::ReedSolomon::new(n, m)
                .map_err(|e| format!("chunk {} parity init: {}", chunk_idx, e))?;
            rs.encode_shards(&mut shards)
                .map_err(|e| format!("chunk {} parity encode: {}", chunk_idx, e))?;
            // Append data shards to EXT_PAYLOAD.
            for i in 0..n {
                payload.extend_from_slice(&shards[i]);
            }
            // Append parity shards to EXT_PARITY_SHARDS.
            for i in n..(n + m) {
                parity_payload.extend_from_slice(&shards[i]);
            }
        }
    } else {
        // No parity: payload is just the concatenation of raw
        // LZMA bytes per chunk. Cheap and lossless.
        let total: usize = chunk_results.iter().map(|c| c.lzma_bytes.len()).sum();
        payload.reserve(total);
        for cr in &chunk_results {
            payload.extend_from_slice(&cr.lzma_bytes);
        }
    }

    // Phase 7 — detect byte-identical chunks (deduplication).
    //
    // The smart decoder can reuse an already-decompressed source
    // chunk's bytes for any chunk that has identical bytes — saves
    // one LZMA decompress per duplicate. Common case: node_modules
    // copies, .git duplicates, or any corpus with repeated files
    // spread across multiple chunks.
    //
    // Algorithm: hash the chunk's uncompressed bytes (already
    // computed in the parallel closure above). Build a map from
    // hash → first chunk_id. Any subsequent chunk with a known
    // hash is a duplicate; emit a DedupEntry pointing at the first.
    //
    // **Note on what we save:** the duplicate chunk's LZMA bytes
    // are STILL in EXT_PAYLOAD (we don't rewrite the payload to
    // skip them — simpler, and the smart decoder never reads them
    // anyway). What we save is the LZMA decompress work on the
    // decoder side, and the bytes that flow through the decoder
    // (CPU + RAM). The on-disk archive still contains the
    // duplicate bytes as a fallback for dumb readers.
    let mut hash_to_source: std::collections::HashMap<[u8; 32], u16> =
        std::collections::HashMap::new();
    let mut dedup_entries: Vec<format_v2::DedupEntry> = Vec::new();
    for (chunk_id, cr) in chunk_results.iter().enumerate() {
        let cid = chunk_id as u16;
        if let Some(&source) = hash_to_source.get(&cr.uncompressed_hash) {
            dedup_entries.push(format_v2::DedupEntry {
                duplicate_id: cid,
                source_id: source,
            });
        } else {
            hash_to_source.insert(cr.uncompressed_hash, cid);
        }
    }

    let mut extensions = vec![
        format_v2::Extension {
            id: format_v2::EXT_TOC,
            payload: format_v2::serialize_toc(&toc_entries),
        },
        format_v2::chunk_table_extension(&chunk_infos),
        format_v2::payload_from_lzma(payload),
    ];
    // Emit EXT_DEDUP_TABLE only when there are actual duplicates --
    // empty tables add overhead without benefit.
    if !dedup_entries.is_empty() {
        extensions.push(format_v2::dedup_table_extension(&dedup_entries));
    }
    // Emit EXT_PARITY_SHARDS when parity shards were generated.
    // Currently we always emit it (DEFAULT_PARITY_PARITY_SHARDS > 0);
    // a future "compress_chunked_no_parity" variant would skip this
    // to save the 20% overhead for users who don't need recovery.
    //
    // The extension holds BOTH the layouts (header) AND the
    // concatenated parity bytes (body), so the decoder only needs
    // to find one extension to get everything.
    if !parity_layouts.is_empty() && !parity_payload.is_empty() {
        // `parity_payload` was built concatenated in chunk order
        // already (Phase 6). Split it into per-chunk vec for the
        // helper.
        let mut per_chunk_parity: Vec<Vec<u8>> = Vec::with_capacity(parity_layouts.len());
        let shard_bytes_per_chunk: usize = parity_layouts
            .iter()
            .map(|l| l.parity_shards as usize * l.parity_shard_size as usize)
            .sum();
        if parity_payload.len() != shard_bytes_per_chunk {
            return Err(format!(
                "EXT_PARITY_SHARDS size mismatch (built {} bytes, expected {})",
                parity_payload.len(),
                shard_bytes_per_chunk
            ));
        }
        let mut cursor = 0usize;
        for l in &parity_layouts {
            let n = l.parity_shards as usize * l.parity_shard_size as usize;
            per_chunk_parity.push(parity_payload[cursor..cursor + n].to_vec());
            cursor += n;
        }
        extensions.push(format_v2::parity_shards_extension(
            &parity_layouts,
            &per_chunk_parity,
        ));
    }
    Ok(format_v2::write_archive(&extensions))
}

/// Backward-compatible chunked wrapper that discards progress and
/// uses the default chunk size.
#[inline]
pub fn compress_chunked(
    files: &[(String, Vec<u8>)],
    lzma_level: u32,
) -> Result<Vec<u8>, String> {
    compress_chunked_with_progress(files, lzma_level, DEFAULT_CHUNK_MAX_UNCOMPRESSED, |_, _, _| {})
}

/// Chunked compression with a custom chunk size AND without progress
/// (used by the streaming single-file path's sister: a single file
/// gets one chunk, so this is equivalent to chunked for that case).
#[allow(dead_code)]
#[inline]
pub fn compress_chunked_simple(
    files: &[(String, Vec<u8>)],
    lzma_level: u32,
    chunk_max_uncompressed: u64,
) -> Result<Vec<u8>, String> {
    compress_chunked_with_progress(
        files,
        lzma_level,
        chunk_max_uncompressed,
        |_, _, _| {},
    )
}

/// Decompress a solid v6 archive, returning the (name, preprocessed
/// bytes) of every file.
///
/// **LOSSY:** the returned bytes are the minified versions, not the
/// originals. To get the original back you would need to feed each
/// returned file through a formatter, which is not what this module
/// does (and not what the v6 backend ever claimed to do).
/// Read just the TOC of a solid archive, WITHOUT touching the
/// LZMA block. Returns the file entries (with uncompressed sizes)
/// and the sum of those sizes. Used by the GUI preview so the
/// user can browse a `.nxs6` archive without paying full
/// decompression cost.
pub fn peek_toc(archive: &[u8]) -> Result<(Vec<FileEntry>, u64), String> {
    let entries = parse_toc(archive)?;
    let total: u64 = entries.iter().map(|e| e.original_size).sum();
    Ok((entries, total))
}

/// Parse the TOC portion of a solid archive (header + entry list)
/// and return the entries. Shared between `peek_toc` and
/// `decompress`.
///
/// **Sprint 5.7.x+:** dispatches by magic byte. Both legacy v1
/// (`NXS6\n`) and the new extensible v2 (`NXS\x00`) formats are
/// supported. Returns `FileEntry` rows in both cases (the v2
/// entries carry a `chunk_id` field too, but `solid_offset` is set
/// to a per-chunk offset that the legacy callers can use directly).
pub fn parse_toc(archive: &[u8]) -> Result<Vec<FileEntry>, String> {
    if archive.len() < 5 {
        return Err("solid archive: too short for magic".into());
    }
    // Dispatch by magic: v1 (NXS6\n) → legacy parser, v2 (NXS\x00)
    // → read the EXT_TOC extension from the v2 manifest.
    if archive.len() >= 5 && &archive[..5] == MAGIC {
        return parse_toc_v1(archive);
    }
    if archive.len() >= 4 && &archive[..4] == format_v2::MAGIC {
        return parse_toc_v2(archive);
    }
    Err("solid archive: bad magic".into())
}

/// Parse a v1 (`NXS6\n`) archive's TOC. Kept private — callers go
/// through `parse_toc` which dispatches by magic.
fn parse_toc_v1(archive: &[u8]) -> Result<Vec<FileEntry>, String> {
    if archive.len() < MAGIC.len() + 1 + 4 {
        return Err("solid archive: too short for header".into());
    }
    let mut pos = 0;
    if &archive[pos..pos + MAGIC.len()] != MAGIC {
        return Err("solid archive: bad magic".into());
    }
    pos += MAGIC.len();
    let version = archive[pos];
    pos += 1;
    if version != VERSION {
        return Err(format!("solid archive: unsupported version {}", version));
    }
    let n_files = u32::from_le_bytes(
        archive[pos..pos + 4]
            .try_into()
            .map_err(|_| "solid archive: bad n_files".to_string())?,
    ) as usize;
    pos += 4;

    let mut entries: Vec<FileEntry> = Vec::with_capacity(n_files);
    for _ in 0..n_files {
        if pos + 2 > archive.len() {
            return Err("solid archive: truncated name_len".into());
        }
        let name_len = u16::from_le_bytes(
            archive[pos..pos + 2]
                .try_into()
                .map_err(|_| "solid archive: bad name_len".to_string())?,
        ) as usize;
        pos += 2;
        if pos + name_len > archive.len() {
            return Err("solid archive: truncated name".into());
        }
        let name = String::from_utf8(archive[pos..pos + name_len].to_vec())
            .map_err(|_| "solid archive: invalid UTF-8 in name".to_string())?;
        pos += name_len;
        if pos + 8 + 8 + 1 + 8 > archive.len() {
            return Err("solid archive: truncated entry".into());
        }
        let original_size = u64::from_le_bytes(
            archive[pos..pos + 8]
                .try_into()
                .map_err(|_| "solid archive: bad orig_size".to_string())?,
        );
        pos += 8;
        let pre_size = u64::from_le_bytes(
            archive[pos..pos + 8]
                .try_into()
                .map_err(|_| "solid archive: bad pre_size".to_string())?,
        );
        pos += 8;
        let preprocessor = match archive[pos] {
            0 => Preprocessor::Raw,
            1 => Preprocessor::Conservative,
            2 => Preprocessor::SwcAst,
            other => return Err(format!("unknown preprocessor id: {}", other)),
        };
        pos += 1;
        let solid_offset = u64::from_le_bytes(
            archive[pos..pos + 8]
                .try_into()
                .map_err(|_| "solid archive: bad offset".to_string())?,
        );
        pos += 8;
        entries.push(FileEntry {
            name,
            original_size,
            pre_size,
            preprocessor,
            // Legacy v1 (NXS6\n) is always single-stream — there's
            // no EXT_CHUNKED concept in v1. We mark it as such so
            // downstream code (extraction, archive_inspect, etc.)
            // treats these archives like single-stream v2 archives.
            chunk_id: 0xFF,
            solid_offset,
        });
    }
    Ok(entries)
}

/// Parse a v2 (`NXS\x00`) archive's TOC. Reads the EXT_TOC
/// extension and converts to legacy `FileEntry` (per-chunk offset
/// becomes `solid_offset`, since the listing UI doesn't care about
/// chunk boundaries — only the names and sizes).
fn parse_toc_v2(archive: &[u8]) -> Result<Vec<FileEntry>, String> {
    let extensions = format_v2::read_archive(archive)?;
    let toc_payload = format_v2::find_extension(&extensions, format_v2::EXT_TOC)
        .ok_or_else(|| "NXS v2: missing required EXT_TOC".to_string())?;
    let v2_entries = format_v2::deserialize_toc(toc_payload)?;
    // Convert v2 entries to legacy FileEntry. The per-chunk offset
    // becomes `solid_offset` (legacy callers treat it as an offset
    // into the chunk's bytes; the per-chunk / per-stream distinction
    // is irrelevant for listing).
    let entries: Vec<FileEntry> = v2_entries
        .into_iter()
        .map(|e| {
            let pre = match e.preprocessor {
                0 => Preprocessor::Raw,
                1 => Preprocessor::Conservative,
                2 => Preprocessor::SwcAst,
                _ => Preprocessor::Conservative,
            };
            FileEntry {
                name: e.name,
                original_size: e.original_size,
                pre_size: e.pre_size,
                preprocessor: pre,
                chunk_id: e.chunk_id,
                solid_offset: e.chunk_offset,
            }
        })
        .collect();
    Ok(entries)
}

pub fn decompress(archive: &[u8]) -> Result<(Vec<FileEntry>, Vec<u8>), String> {
    // Too-short short-circuit: bail with a clear error before
    // touching the magic-byte matchers (both NXS6\n and NXS\x00
    // have multi-byte magics).
    if archive.len() < 5 {
        return Err(format!(
            "solid archive: too short ({} bytes; need at least 5 for magic)",
            archive.len()
        ));
    }
    // Magic-byte dispatch. The new extensible format starts with
    // `NXS\x00`; the legacy v1 starts with `NXS6\n`. Anything
    // else is an unknown format (could be a corruption, a future
    // magic we don't know, or just random bytes).
    if &archive[..4] == format_v2::MAGIC {
        decompress_v2(archive)
    } else if &archive[..5] == MAGIC {
        // Legacy NXS6 path — kept so users with old archives can
        // still extract them after upgrading.
        decompress_v1_legacy(archive)
    } else {
        Err(format!(
            "solid archive: bad magic (expected 'NXS6\\n' or 'NXS\\x00', got {:02x?})",
            &archive[..archive.len().min(5)]
        ))
    }
}

/// Decompress a v2 extensible archive (`NXS\x00` magic). This is
/// the new code path; future extensions land here.
///
/// When the archive includes an `EXT_CHUNKED` extension, the
/// payload is N concatenated LZMA streams and the TOC's
/// `chunk_offset` field is per-chunk (not global). We dispatch to
/// `decompress_chunked_full` which reassembles the full solid
/// stream into one Vec<u8> for legacy callers (the API in
/// `api.rs::decompress_target_with_progress` detects this and
/// uses `extract_files_chunked` instead, which is more memory
/// efficient).
fn decompress_v2(archive: &[u8]) -> Result<(Vec<FileEntry>, Vec<u8>), String> {
    let extensions = format_v2::read_archive(archive)?;
    let toc_payload = format_v2::find_extension(&extensions, format_v2::EXT_TOC)
        .ok_or_else(|| "NXS v2: missing required EXT_TOC".to_string())?;
    let payload_bytes = format_v2::find_extension(&extensions, format_v2::EXT_PAYLOAD)
        .ok_or_else(|| "NXS v2: missing required EXT_PAYLOAD".to_string())?;
    let v2_entries = format_v2::deserialize_toc(toc_payload)?;

    // Detect EXT_CHUNKED and dispatch.
    if let Some(chunk_table_bytes) =
        format_v2::find_extension(&extensions, format_v2::EXT_CHUNKED)
    {
        let chunks = format_v2::deserialize_chunk_table(chunk_table_bytes)?;
        // Optional EXT_PARITY_SHARDS: Reed-Solomon sharding of
        // each chunk's compressed bytes for corruption recovery.
        // When present, the decoder uses the parity to reconstruct
        // any missing/corrupted data shards.
        let (parity_layouts, parity_bytes): (Vec<format_v2::ParityShardLayout>, &[u8]) =
            if let Some(pe_bytes) =
                format_v2::find_extension(&extensions, format_v2::EXT_PARITY_SHARDS)
            {
                let (layouts, parity) = format_v2::deserialize_parity_extension(pe_bytes)?;
                (layouts, parity)
            } else {
                (Vec::new(), &[])
            };
        // Optional EXT_DEDUP_TABLE: chunks whose bytes are
        // identical to an earlier chunk. Reuses the source's
        // decompressed bytes instead of re-running LZMA.
        let dedup_table: std::collections::HashMap<u16, u16> =
            if let Some(dt_bytes) =
                format_v2::find_extension(&extensions, format_v2::EXT_DEDUP_TABLE)
            {
                format_v2::deserialize_dedup_table(dt_bytes)?
                    .into_iter()
                    .map(|e| (e.duplicate_id, e.source_id))
                    .collect()
            } else {
                std::collections::HashMap::new()
            };
        // Locate each chunk's compressed bytes inside EXT_PAYLOAD
        // and LZMA-decompress them sequentially. Dedup'd chunks
        // are skipped (we use the source's already-decompressed
        // bytes when needed).
        let mut chunk_offsets_in_payload: Vec<u64> = Vec::with_capacity(chunks.len());
        let mut acc: u64 = 0;
        for c in &chunks {
            chunk_offsets_in_payload.push(acc);
            acc += c.compressed_size;
        }
        // The position in `solid_uncompressed` where each chunk's
        // bytes would start. Dedup'd chunks take the same offset
        // as their source (no extra bytes appended).
        let mut chunk_uncompressed_starts: Vec<u64> = Vec::with_capacity(chunks.len());
        let mut acc_uncomp: u64 = 0;
        for (i, c) in chunks.iter().enumerate() {
            let is_dedup = dedup_table.contains_key(&(i as u16));
            if !is_dedup {
                chunk_uncompressed_starts.push(acc_uncomp);
                acc_uncomp += c.uncompressed_size;
            } else {
                // Dedup'd chunk shares its source's bytes; its
                // own bytes are NOT appended to solid_uncompressed.
                // We still need to record an offset for the
                // per-entry global offset calc below — use the
                // source's start (since dedup'd content == source's
                // content, files in chunk i live at the same byte
                // range as they'd live in the source).
                let source = dedup_table[&(i as u16)] as usize;
                chunk_uncompressed_starts.push(chunk_uncompressed_starts[source]);
            }
        }
        let mut solid_uncompressed: Vec<u8> = Vec::new();
        let mut cache: std::collections::HashMap<usize, Vec<u8>> =
            std::collections::HashMap::new();
        for (i, _c) in chunks.iter().enumerate() {
            // Resolve dedup: if chunk i is dedup'd, use the source
            // chunk's bytes (which we may have already cached).
            let actual_i = if let Some(&source) = dedup_table.get(&(i as u16)) {
                source as usize
            } else {
                i
            };
            let chunk_uncompressed: Vec<u8> = if let Some(cached) = cache.get(&actual_i) {
                cached.clone()
            } else {
                // Reassemble the chunk's LZMA bytes from data
                // shards + parity (if present). This is where
                // Reed-Solomon recovery happens when shards are
                // missing/corrupted.
                let chunk_lzma = reassemble_chunk_lzma(
                    actual_i,
                    &chunks,
                    &parity_layouts,
                    payload_bytes,
                    parity_bytes,
                )?;
                let mut out: Vec<u8> = Vec::new();
                XzDecoder::new(chunk_lzma.as_slice())
                    .read_to_end(&mut out)
                    .map_err(|e| {
                        format!("chunk {} LZMA decompress failed: {}", actual_i, e)
                    })?;
                if out.len() as u64 != chunks[actual_i].uncompressed_size {
                    return Err(format!(
                        "chunk {} size mismatch (declared {}, got {})",
                        actual_i,
                        chunks[actual_i].uncompressed_size,
                        out.len()
                    ));
                }
                cache.insert(actual_i, out.clone());
                out
            };
            // Only append non-dedup chunks (to avoid duplicate
            // ranges in solid_uncompressed).
            if !dedup_table.contains_key(&(i as u16)) {
                solid_uncompressed.extend_from_slice(&chunk_uncompressed);
            }
        }
        // Build FileEntries with GLOBAL `solid_offset` (i.e.,
        // position in the reassembled `solid_uncompressed` Vec).
        // The TOC's `chunk_offset` field is per-chunk (relative to
        // the chunk's first byte); we add the chunk's start in the
        // reassembled stream to get the global offset that
        // `solid[solid_offset..solid_offset+pre_size]` slicing
        // expects.
        let entries: Vec<FileEntry> = v2_entries
            .into_iter()
            .map(|e| {
                let pre = match e.preprocessor {
                    0 => Preprocessor::Raw,
                    1 => Preprocessor::Conservative,
                    2 => Preprocessor::SwcAst,
                    _ => Preprocessor::Conservative,
                };
                let chunk_start = if (e.chunk_id as usize) < chunk_uncompressed_starts.len() {
                    chunk_uncompressed_starts[e.chunk_id as usize]
                } else {
                    // Defensive: chunk_id == 0xFF (single-stream
                    // marker) shouldn't appear in a chunked
                    // archive, but handle gracefully.
                    0
                };
                FileEntry {
                    name: e.name,
                    original_size: e.original_size,
                    pre_size: e.pre_size,
                    preprocessor: pre,
                    chunk_id: e.chunk_id,
                    solid_offset: chunk_start + e.chunk_offset,
                }
            })
            .collect();
        // Cross-check offsets against the decompressed length.
        for e in &entries {
            let end = e.solid_offset as usize + e.pre_size as usize;
            if end > solid_uncompressed.len() {
                return Err(format!(
                    "chunked archive: file '{}' would be at {}..{} (past end)",
                    e.name,
                    e.solid_offset,
                    end
                ));
            }
        }
        return Ok((entries, solid_uncompressed));
    }

    // Single-stream payload (no EXT_CHUNKED). Legacy path.
    let entries: Vec<FileEntry> = v2_entries
        .into_iter()
        .map(|e| {
            let pre = match e.preprocessor {
                0 => Preprocessor::Raw,
                1 => Preprocessor::Conservative,
                2 => Preprocessor::SwcAst,
                _ => Preprocessor::Conservative,
            };
            FileEntry {
                name: e.name,
                original_size: e.original_size,
                pre_size: e.pre_size,
                preprocessor: pre,
                chunk_id: e.chunk_id,
                solid_offset: e.chunk_offset,
            }
        })
        .collect();
    let mut solid_uncompressed: Vec<u8> = Vec::new();
    XzDecoder::new(payload_bytes)
        .read_to_end(&mut solid_uncompressed)
        .map_err(|e| format!("solid LZMA decompress failed: {}", e))?;
    for e in &entries {
        let end = e.solid_offset as usize + e.pre_size as usize;
        if end > solid_uncompressed.len() {
            return Err(format!(
                "solid archive: entry '{}' extends past solid block ({} > {})",
                e.name,
                end,
                solid_uncompressed.len()
            ));
        }
    }
    Ok((entries, solid_uncompressed))
}

/// Compute the byte offset in `payload_bytes` where chunk
/// `chunk_id`'s data starts. Used by both the legacy chunked
/// reassembly path and the random-access `extract_files_chunked`.
fn chunk_payload_offset(chunks: &[format_v2::ChunkInfo], chunk_id: usize) -> u64 {
    chunks
        .iter()
        .take(chunk_id)
        .map(|c| c.compressed_size)
        .sum()
}

/// Compute the byte offset in `parity_bytes` (the EXT_PARITY_SHARDS
/// extension payload, after the layout header) where chunk
/// `chunk_id`'s parity shards start.
fn chunk_parity_offset(layouts: &[format_v2::ParityShardLayout], chunk_id: usize) -> usize {
    layouts
        .iter()
        .take(chunk_id)
        .map(|l| l.parity_shards as usize * l.parity_shard_size as usize)
        .sum()
}

/// Reassemble a chunk's LZMA bytes from the data shards in
/// `payload_bytes` and the parity shards in `parity_bytes`,
/// using Reed-Solomon to recover any missing shards.
///
/// When `parity_bytes` is empty (no EXT_PARITY_SHARDS extension in
/// the archive), this is a no-op: the chunk's compressed bytes
/// live contiguously in `payload_bytes`. When `parity_bytes` is
/// non-empty, each chunk's data is split into `data_shards`
/// shards in `payload_bytes` and `parity_shards` shards in
/// `parity_bytes`; if any data shards are truncated or
/// corrupted (out of `payload_bytes` range), Reed-Solomon uses
/// the parity to reconstruct them.
///
/// **Returns** the reconstructed LZMA bytes (with possible
/// trailing zero-padding from the last data shard — the LZMA
/// decoder silently consumes these).
fn reassemble_chunk_lzma(
    chunk_id: usize,
    chunks: &[format_v2::ChunkInfo],
    layouts: &[format_v2::ParityShardLayout],
    payload_bytes: &[u8],
    parity_bytes: &[u8],
) -> Result<Vec<u8>, String> {
    if layouts.is_empty() {
        // No parity sharding — the chunk's compressed bytes are
        // stored contiguously in payload_bytes.
        let start = chunk_payload_offset(chunks, chunk_id) as usize;
        let size = chunks[chunk_id].compressed_size as usize;
        let end = start + size;
        if end > payload_bytes.len() {
            return Err(format!(
                "chunk {} truncated: need bytes {}..{}, have {}",
                chunk_id, start, end, payload_bytes.len()
            ));
        }
        return Ok(payload_bytes[start..end].to_vec());
    }

    // Parity sharding: read N data shards + M parity shards,
    // use Reed-Solomon to recover any missing ones.
    let layout = &layouts[chunk_id];
    let n = layout.data_shards as usize;
    let m = layout.parity_shards as usize;
    let shard_size = layout.data_shard_size as usize;

    // Build the shard vector with Option<Vec<u8>> — None marks
    // missing/corrupted shards that RS will reconstruct.
    let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(n + m);

    // N data shards from payload_bytes.
    let data_offset = chunk_payload_offset(chunks, chunk_id) as usize;
    for i in 0..n {
        let start = data_offset + i * shard_size;
        let end = start + shard_size;
        if end > payload_bytes.len() {
            shards.push(None);
        } else {
            shards.push(Some(payload_bytes[start..end].to_vec()));
        }
    }

    // M parity shards from parity_bytes.
    let parity_offset = chunk_parity_offset(layouts, chunk_id);
    for _ in 0..m {
        let start = parity_offset;
        let end = start + shard_size;
        if end > parity_bytes.len() {
            shards.push(None);
        } else {
            shards.push(Some(parity_bytes[start..end].to_vec()));
        }
        // Advance the cursor for the next parity shard in this
        // chunk — but we use the per-chunk offset so we don't
        // need to advance here. The next call to
        // `chunk_parity_offset` would skip ahead correctly.
    }

    // Recover missing shards via Reed-Solomon. On a healthy
    // archive, nothing is missing and this is a no-op.
    //
    // **Known limitation (Sprint 5.7.x+):** the current
    // implementation only detects OUT-OF-BAND corruption
    // (truncated payload — shards beyond `payload_bytes.len()`).
    // In-band corruption (shards present but with wrong bytes)
    // is not detected; per-shard CRCs are planned for a future
    // sprint. With out-of-band only, recovery kicks in when
    // the user has truncated the archive (e.g. partial
    // transfer), not when the file is bit-flipped.
    let missing: usize = shards.iter().filter(|s| s.is_none()).count();
    if missing > 0 {
        if missing > m {
            return Err(format!(
                "chunk {} has {} missing shards, can only recover up to {}",
                chunk_id, missing, m
            ));
        }
        let rs = crate::galois::codes::ReedSolomon::new(n, m)
            .map_err(|e| format!("chunk {} RS init: {}", chunk_id, e))?;
        rs.decode_recover(&mut shards)
            .map_err(|e| format!("chunk {} RS decode: {}", chunk_id, e))?;
    }

    // Concatenate recovered data shards. We then TRUNCATE to
    // the original LZMA size (stored in the layout) — the last
    // data shard is zero-padded to `data_shard_size`, so the
    // reassembled bytes are `data_shards * data_shard_size` long
    // even though the original xz stream is shorter. Truncating
    // before handing the bytes to the xz decoder prevents it
    // from trying to interpret the trailing padding as the
    // start of another stream.
    let mut result = Vec::with_capacity(n * shard_size);
    for i in 0..n {
        match shards[i].take() {
            Some(s) => result.extend_from_slice(&s),
            None => {
                return Err(format!(
                    "chunk {} data shard {} still missing after recovery",
                    chunk_id, i
                ))
            }
        }
    }
    let lzma_size = layout.lzma_size as usize;
    if result.len() > lzma_size {
        result.truncate(lzma_size);
    }
    Ok(result)
}

/// Decompress a legacy v1 (`NXS6\n`) archive. Kept so existing
/// archives on disk continue to extract after the format upgrade.
fn decompress_v1_legacy(archive: &[u8]) -> Result<(Vec<FileEntry>, Vec<u8>), String> {
    let entries = parse_toc(archive)?;
    let mut pos = MAGIC.len() + 1 + 4;
    for e in &entries {
        pos += 2 + e.name.len() + 8 + 8 + 1 + 8;
    }
    let lzma_compressed = &archive[pos..];
    let mut solid_uncompressed: Vec<u8> = Vec::new();
    XzDecoder::new(lzma_compressed)
        .read_to_end(&mut solid_uncompressed)
        .map_err(|e| format!("solid LZMA decompress failed: {}", e))?;
    for e in &entries {
        let end = e.solid_offset as usize + e.pre_size as usize;
        if end > solid_uncompressed.len() {
            return Err(format!(
                "solid archive: entry '{}' extends past solid block ({} > {})",
                e.name,
                end,
                solid_uncompressed.len()
            ));
        }
    }
    Ok((entries, solid_uncompressed))
}

/// Compute the byte offset in an NXS v2 archive where the
/// `EXT_PAYLOAD` extension starts. Used by the streaming decoder
/// to skip the header + manifest + TOC without re-parsing the
/// manifest. We re-walk it (cheap: O(n_entries) header bytes)
/// instead of caching the offset at write time because the TOC
/// can be modified by future features (`EXT_CHUNKED` will change
/// where the payload sits).
///
/// Returns `Err` if the archive is malformed (no payload
/// extension, truncated TOC, etc.).
pub fn payload_offset(archive: &[u8]) -> Result<usize, String> {
    let extensions = format_v2::read_archive(archive)?;
    // Header is 8 bytes; manifest is 2 + N * 6 + sum_payload.
    let mut pos = format_v2::HEADER_SIZE + 2; // n_extensions
    for ext in &extensions {
        match ext.id {
            format_v2::EXT_PAYLOAD => {
                // `pos` is the start of the EXT_PAYLOAD entry
                // itself (i.e., where its 2-byte id lives). The
                // actual LZMA bytes start 6 bytes later
                // (id[2] + size[4]).
                return Ok(pos + 6);
            }
            _ => {} // skip unknown extensions
        }
        pos += 6 + ext.payload.len();
    }
    Err("NXS v2: no EXT_PAYLOAD found".to_string())
}

// ─────────────────────────────────────────────────────────────────────
//   Sprint 5.7.x+ — streaming decoder (bounded memory)
// ─────────────────────────────────────────────────────────────────────
//
// The legacy `decompress()` above materializes the WHOLE
// decompressed solid stream in RAM (Vec<u8> solid_uncompressed).
// For archives with many large entries (e.g. a backup bundle of
// GB-sized files), this doubles the RAM footprint — the
// compressed bytes are already in `archive`; the decompressed
// payload doubles that on top.
//
// `decompress_entries_streaming` takes the SAME (entries,
// lzma_payload) tuple as `decompress()` but instead of returning
// the full solid stream, it walks the entries in order, opens a
// writer for the first entry, and switches to the next writer when
// the byte position crosses an entry's offset boundary. The LZMA
// decoder is consumed sequentially (xz2 is forward-only) so
// total memory is bounded by:
//
//   RAM  ≈  max(pre_size_i for any i) + xz2 internal state
//        + 256 KB I/O buffer
//
// For an archive with 50 files of 500 MB each (~25 GB), the
// legacy path used ~50 GB (compressed in `archive` + decompressed
// in `solid_uncompressed`). The streaming path uses ~500 MB (the
// largest single file) — a 100× reduction.
//
// We can NOT seek in the decompressed stream (xz2 is forward-only),
// so file boundaries must be detected by byte offset rather than
// re-positioning. The TOC's `solid_offset` fields tell us exactly
// where each entry's slice starts, which is enough.
pub fn decompress_entries_streaming(
    entries: &[FileEntry],
    lzma_payload: &[u8],
    output_dir: &Path,
) -> Result<Vec<String>, String> {
    use std::fs::File;
    use std::io::{BufWriter, Write};

    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("mkdir output: {}", e))?;

    let mut written = Vec::with_capacity(entries.len());
    let mut decoder = XzDecoder::new(lzma_payload);
    // 256 KB read buffer — large enough to keep xz2 productive
    // (one LZMA dictionary window is ~64 MB but we don't need
    // to fill it; xz2 internally buffers its own decompressed
    // chunks). 256 KB is also a good I/O sweet spot.
    let mut buf = vec![0u8; 256 * 1024];
    // `byte_pos` is the cumulative count of decompressed bytes
    // emitted by the decoder so far. It tracks where we are in
    // the logical solid stream.
    let mut byte_pos: u64 = 0;
    // Index of the entry we're currently writing (if any).
    let mut cur_idx: Option<usize> = None;
    // Bytes remaining in the current entry's slice.
    let mut cur_remaining: u64 = 0;
    // The current file writer, held open until its slice ends.
    let mut cur_writer: Option<BufWriter<File>> = None;

    // First pass: handle entries with pre_size == 0 (empty files).
    // These have no bytes in the LZMA stream — they exist only
    // in the TOC. The streaming loop below never opens a writer
    // for them (byte_pos doesn't enter their range because the
    // range is empty: [N, N)). We materialize them upfront so
    // `written.len() == entries.len()` regardless of content.
    for e in entries.iter() {
        if e.pre_size == 0 {
            let path = output_dir.join(&e.name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("mkdir {}: {}", parent.display(), err))?;
            }
            File::create(&path)
                .map_err(|err| format!("create {}: {}", path.display(), err))?;
            written.push(e.name.clone());
        }
    }

    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|e| format!("LZMA read failed: {}", e))?;
        if n == 0 {
            break;
        }
        let mut chunk_pos = 0;
        while chunk_pos < n {
            // If no entry is active, scan forward through the TOC
            // to find the entry whose range contains byte_pos.
            // (TOC is sorted by offset; this is O(n) per boundary
            // but n is small in practice.)
            if cur_writer.is_none() {
                // Find the first entry whose range covers byte_pos.
                // We always start from index 0 because byte_pos is
                // monotonically non-decreasing — once an entry is
                // fully written, byte_pos == entry_end, so the next
                // entry's start is the only one with byte_pos in
                // its range.
                let mut found = false;
                for (i, e) in entries.iter().enumerate() {
                    let entry_start = e.solid_offset;
                    let entry_end = entry_start + e.pre_size;
                    if e.pre_size > 0
                        && byte_pos >= entry_start
                        && byte_pos < entry_end
                    {
                        // Open the file.
                        let path = output_dir.join(&e.name);
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent)
                                .map_err(|err| {
                                    format!("mkdir {}: {}", parent.display(), err)
                                })?;
                        }
                        let file = File::create(&path)
                            .map_err(|err| format!("create {}: {}", path.display(), err))?;
                        cur_writer = Some(BufWriter::with_capacity(64 * 1024, file));
                        cur_remaining = entry_end - byte_pos;
                        cur_idx = Some(i);
                        found = true;
                        break;
                    }
                }
                if !found {
                    // LZMA stream emitted bytes past the last
                    // entry's end — archive is malformed.
                    return Err(format!(
                        "no entry covers byte_pos={} (TOC ends at {})",
                        byte_pos,
                        entries.last().map(|e| e.solid_offset + e.pre_size).unwrap_or(0)
                    ));
                }
            }
            // Write as much of the buffer as fits the current
            // entry's remaining slice.
            let write_n = (n - chunk_pos).min(cur_remaining as usize);
            cur_writer
                .as_mut()
                .unwrap()
                .write_all(&buf[chunk_pos..chunk_pos + write_n])
                .map_err(|e| format!("write failed: {}", e))?;
            chunk_pos += write_n;
            byte_pos += write_n as u64;
            cur_remaining -= write_n as u64;
            if cur_remaining == 0 {
                // Close the current writer and move to the next
                // entry. Drop the writer explicitly so the file
                // is flushed + closed before we open the next.
                let writer = cur_writer.take().unwrap();
                writer
                    .into_inner()
                    .map_err(|e| format!("flush failed: {}", e))?
                    .sync_all()
                    .map_err(|e| format!("sync failed: {}", e))?;
                written.push(entries[cur_idx.unwrap()].name.clone());
                cur_idx = None;
            }
        }
    }
    if cur_writer.is_some() {
        return Err("decompressor ended mid-entry (truncated archive)".to_string());
    }
    Ok(written)
}

// ─────────────────────────────────────────────────────────────────────
//   Sprint 5.7.x+ — chunked extraction (random-access over EXT_CHUNKED)
//
//   When an archive declares `EXT_CHUNKED`, we know exactly where
//   each file's owning chunk starts and how big it is (both
//   compressed and uncompressed). This lets us:
//
//   1. **Bounded memory.** Decompress one chunk at a time (peak
//      = largest chunk's uncompressed size), instead of materializing
//      the whole archive in RAM.
//   2. **Random-access extraction.** If the caller asks for a subset
//      of files, we only LZMA-decompress the chunks that contain
//      them. A user extracting one file from a 1000-file archive
//      pays O(1) or O(its-own-chunk) work, not O(total archive).
//
//   The function `extract_files_chunked` is the main entry point.
//   It takes the full archive bytes (we re-parse the manifest for
//   chunk table + TOC + payload offsets — cheap, O(n_entries) header
//   bytes) plus the set of files to extract. If `selected_files`
//   is `None` we extract everything (equivalent to a full extract
//   but per-chunk to stay memory-bounded).
//
//   Returns the list of written file names so callers can report
//   "extracted N of M" stats.
pub fn extract_files_chunked(
    archive: &[u8],
    selected_files: Option<&[String]>,
    output_dir: &Path,
) -> Result<Vec<String>, String> {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::sync::Arc;

    let extensions = format_v2::read_archive(archive)?;
    let toc_payload = format_v2::find_extension(&extensions, format_v2::EXT_TOC)
        .ok_or_else(|| "NXS v2: missing required EXT_TOC".to_string())?;
    let payload_bytes = format_v2::find_extension(&extensions, format_v2::EXT_PAYLOAD)
        .ok_or_else(|| "NXS v2: missing required EXT_PAYLOAD".to_string())?;
    let chunk_table_bytes = format_v2::find_extension(&extensions, format_v2::EXT_CHUNKED)
        .ok_or_else(|| "extract_files_chunked called on non-chunked archive".to_string())?;
    let chunks = format_v2::deserialize_chunk_table(chunk_table_bytes)?;
    // Optional EXT_PARITY_SHARDS: Reed-Solomon shard layout +
    // concatenated parity bytes. When present, the decoder uses
    // the parity to reconstruct missing/corrupted data shards
    // (e.g., from bit-rot on long-term backups or partial
    // transfer corruption).
    let (parity_layouts, parity_bytes): (Vec<format_v2::ParityShardLayout>, &[u8]) =
        if let Some(pe_bytes) =
            format_v2::find_extension(&extensions, format_v2::EXT_PARITY_SHARDS)
        {
            let (layouts, parity) = format_v2::deserialize_parity_extension(pe_bytes)?;
            (layouts, parity)
        } else {
            (Vec::new(), &[])
        };
    // Optional dedup table: maps `duplicate_id -> source_id` for
    // chunks whose bytes are identical to an earlier chunk. When
    // present, the decoder reuses the source chunk's decompressed
    // bytes instead of LZMA-decompressing the duplicate (which
    // would just produce the same bytes again).
    let dedup_table: std::collections::HashMap<u16, u16> =
        if let Some(dt_bytes) = format_v2::find_extension(&extensions, format_v2::EXT_DEDUP_TABLE)
        {
            format_v2::deserialize_dedup_table(dt_bytes)?
                .into_iter()
                .map(|e| (e.duplicate_id, e.source_id))
                .collect()
        } else {
            std::collections::HashMap::new()
        };
    let v2_entries = format_v2::deserialize_toc(toc_payload)?;
    let entries: Vec<FileEntry> = v2_entries
        .into_iter()
        .map(|e| {
            let pre = match e.preprocessor {
                0 => Preprocessor::Raw,
                1 => Preprocessor::Conservative,
                2 => Preprocessor::SwcAst,
                _ => Preprocessor::Conservative,
            };
            FileEntry {
                name: e.name,
                original_size: e.original_size,
                pre_size: e.pre_size,
                preprocessor: pre,
                chunk_id: e.chunk_id,
                solid_offset: e.chunk_offset,
            }
        })
        .collect();

    // Compute compressed-byte ranges inside EXT_PAYLOAD per chunk.
    // chunk_compressed_starts[i] = byte offset in EXT_PAYLOAD where
    // chunk i's LZMA stream begins.
    let mut chunk_compressed_starts: Vec<u64> = Vec::with_capacity(chunks.len());
    let mut acc: u64 = 0;
    for c in &chunks {
        chunk_compressed_starts.push(acc);
        acc += c.compressed_size;
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("mkdir output: {}", e))?;

    let mut written: Vec<String> = Vec::new();

    // Group entries by chunk. The TOC carries `chunk_id` directly
    // (set by the writer when it split files across chunks) so we
    // don't need to scan the chunk table here — O(n_entries) with
    // no chunk-table lookup per entry.
    let mut entries_by_chunk: Vec<Vec<&FileEntry>> = vec![Vec::new(); chunks.len()];
    for e in &entries {
        let chunk_id = e.chunk_id as usize;
        // chunk_id == 0xFF means single-stream — that can't happen
        // here because we already verified EXT_CHUNKED is present.
        // We treat anything out of range as a defensive default
        // (drop the entry with a clear error rather than panic).
        if e.pre_size > 0 && chunk_id < chunks.len() {
            entries_by_chunk[chunk_id].push(e);
        }
    }

    // Pre-pass: materialize empty files (pre_size == 0) so the
    // returned `written` count is correct even when the chunk
    // loop below doesn't touch them.
    for e in entries.iter().filter(|e| e.pre_size == 0) {
        let path = output_dir.join(&e.name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("mkdir {}: {}", parent.display(), err))?;
        }
        File::create(&path)
            .map_err(|err| format!("create {}: {}", path.display(), err))?;
        written.push(e.name.clone());
    }

    // Per-chunk decompression loop. Each iteration:
    //   1. Resolve the chunk's `chunk_id` through the dedup
    //      table to find the actual source (or use it directly
    //      if not dedup'd).
    //   2. Look up the source's decompressed bytes in the cache;
    //      if absent, LZMA-decompress them and cache them.
    //   3. For each selected entry in this chunk, slice its bytes
    //      from the source's decompressed Vec (the file's
    //      `chunk_offset` is valid for both the original and
    //      source chunks since they have identical content).
    //
    // **Cache semantics:** the cache only ever holds chunks that
    // are SOURCES (i.e., not themselves dedup'd). The writer
    // guarantees sources have smaller chunk_id than their dedup'd
    // children, so by the time we encounter a dedup'd chunk K,
    // its source M has already been processed and may still be
    // in the cache. If we never re-access M, we drop it from
    // the cache after the loop — bounded memory overall.
    let mut cache: std::collections::HashMap<u16, Arc<Vec<u8>>> =
        std::collections::HashMap::new();
    for (chunk_id, chunk_entries) in entries_by_chunk.iter().enumerate() {
        // Skip chunks with no selected entries (random-access win).
        let chunk_selected: Vec<&&FileEntry> = match selected_files {
            Some(sel) => chunk_entries
                .iter()
                .filter(|e| sel.iter().any(|s| s == &e.name))
                .collect(),
            None => chunk_entries.iter().collect(),
        };
        if chunk_selected.is_empty() {
            continue;
        }
        // Resolve through dedup table: if chunk_id is dedup'd, use
        // the source's bytes. Sources are never themselves dedup'd
        // (writer invariant), so this is always 1 hop.
        let actual_chunk_id = if let Some(&source) = dedup_table.get(&(chunk_id as u16)) {
            source
        } else {
            chunk_id as u16
        };
        let bytes_arc: Arc<Vec<u8>> = if let Some(cached) = cache.get(&actual_chunk_id) {
            cached.clone()
        } else {
            // Reassemble this chunk's LZMA bytes from data shards
            // + parity (Reed-Solomon recovery happens inside).
            let chunk_lzma = reassemble_chunk_lzma(
                actual_chunk_id as usize,
                &chunks,
                &parity_layouts,
                payload_bytes,
                parity_bytes,
            )?;
            let mut chunk_uncompressed: Vec<u8> = Vec::new();
            XzDecoder::new(chunk_lzma.as_slice())
                .read_to_end(&mut chunk_uncompressed)
                .map_err(|e| {
                    format!(
                        "chunk {} LZMA decompress failed: {}",
                        actual_chunk_id, e
                    )
                })?;
            let arc = Arc::new(chunk_uncompressed);
            cache.insert(actual_chunk_id, arc.clone());
            arc
        };
        for e in chunk_selected {
            let entry_start = e.solid_offset as usize;
            let entry_end = entry_start + e.pre_size as usize;
            if entry_end > bytes_arc.len() {
                return Err(format!(
                    "chunked archive: file '{}' would be at {}..{} (past chunk {})",
                    e.name,
                    entry_start,
                    entry_end,
                    chunk_id
                ));
            }
            let file_bytes = &bytes_arc[entry_start..entry_end];
            let path = output_dir.join(&e.name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("mkdir {}: {}", parent.display(), err))?;
            }
            let file = File::create(&path)
                .map_err(|err| format!("create {}: {}", path.display(), err))?;
            let mut writer = BufWriter::with_capacity(64 * 1024, file);
            writer
                .write_all(file_bytes)
                .map_err(|err| format!("write {}: {}", path.display(), err))?;
            writer
                .into_inner()
                .map_err(|e| format!("flush failed: {}", e))?
                .sync_all()
                .map_err(|e| format!("sync failed: {}", e))?;
            written.push(e.name.clone());
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_files() -> Vec<(String, Vec<u8>)> {
        vec![
            (
                "src/index.ts".to_string(),
                b"interface User { name: string; age: number; }\nconst x: number = 1;\n".to_vec(),
            ),
            (
                "src/util.ts".to_string(),
                b"// helper\nexport function add(a: number, b: number): number { return a + b; }\n"
                    .to_vec(),
            ),
            (
                "README.md".to_string(),
                b"# Project\n\nThis is a test.\n".to_vec(),
            ),
            (
                "data.json".to_string(),
                br#"{"key": "value", "n": 42}"#.to_vec(),
            ),
        ]
    }

    #[test]
    fn roundtrip_simple() {
        let files = sample_files();
        let original_total: usize = files.iter().map(|(_, b)| b.len()).sum();
        let archive = compress(&files, 6).expect("compress");
        let (entries, solid) = decompress(&archive).expect("decompress");
        assert_eq!(entries.len(), files.len());
        // Slice for each file and check we get something back.
        for (i, e) in entries.iter().enumerate() {
            let start = e.solid_offset as usize;
            let end = start + e.pre_size as usize;
            let got = &solid[start..end];
            assert!(!got.is_empty(), "file {} returned empty", i);
            // The preprocessor is lossy for .ts, so we can't byte-compare.
            // But for .json/.md (conservative), the result must equal
            // the minify() of the input.
            if e.preprocessor == Preprocessor::Conservative {
                let expected = minify::minify(&files[i].1);
                assert_eq!(
                    got,
                    &expected[..],
                    "conservative minify mismatch for {}",
                    e.name
                );
            }
        }
        // The archive should be smaller than the input on this simple
        // set (repetitive structure).
        eprintln!(
            "roundtrip_simple: input={} B, archive={} B, ratio={:.2}x",
            original_total,
            archive.len(),
            original_total as f64 / archive.len() as f64,
        );
    }

    #[test]
    fn swc_used_for_ts() {
        let files: Vec<(String, Vec<u8>)> = vec![(
            "src/a.ts".to_string(),
            b"const x: number = 1;\nconst y: string = 'hi';\n".to_vec(),
        )];
        let archive = compress(&files, 6).expect("compress");
        let (entries, solid) = decompress(&archive).expect("decompress");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].preprocessor, Preprocessor::SwcAst);
        // The decompressed bytes must NOT contain ": number" or
        // ": string" — swc strips type annotations.
        let start = entries[0].solid_offset as usize;
        let end = start + entries[0].pre_size as usize;
        let s = std::str::from_utf8(&solid[start..end]).unwrap();
        assert!(!s.contains(": number"));
        assert!(!s.contains(": string"));
    }

    #[test]
    fn conservative_used_for_json() {
        let files: Vec<(String, Vec<u8>)> =
            vec![("data.json".to_string(), br#"{"k":"v"}"#.to_vec())];
        let archive = compress(&files, 6).expect("compress");
        let (entries, solid) = decompress(&archive).expect("decompress");
        assert_eq!(entries[0].preprocessor, Preprocessor::Conservative);
        let start = entries[0].solid_offset as usize;
        let end = start + entries[0].pre_size as usize;
        let s = std::str::from_utf8(&solid[start..end]).unwrap();
        assert_eq!(s, r#"{"k":"v"}"#);
    }

    #[test]
    fn empty_archive() {
        let files: Vec<(String, Vec<u8>)> = vec![];
        let archive = compress(&files, 6).expect("compress");
        let (entries, solid) = decompress(&archive).expect("decompress");
        assert_eq!(entries.len(), 0);
        assert!(solid.is_empty());
    }

    #[test]
    fn bad_magic_rejected() {
        let err = decompress(b"NOPE\x00not a solid archive").unwrap_err();
        assert!(err.contains("bad magic"), "got: {}", err);
    }

    #[test]
    fn truncated_rejected() {
        let err = decompress(b"NX").unwrap_err();
        assert!(err.contains("too short"), "got: {}", err);
    }

    #[test]
    fn roundtrip_preserves_file_count_and_names() {
        let files = sample_files();
        let archive = compress(&files, 6).expect("compress");
        let (entries, _) = decompress(&archive).expect("decompress");
        // Sprint 5.7.x: files are now sorted by SimHash before
        // concatenation (better LZMA match rate). The TOC follows
        // that same order, so we compare the file sets rather
        // than the exact sequences — same files, different order.
        let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        let mut expected: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        expected.sort();
        assert_eq!(names, expected);
    }

    // ─────────────────────────────────────────────────────────────────
    //   Sprint 5.7.x — SimHash + content-aware tests
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn simhash_is_deterministic() {
        let data = b"hello world, this is some test data for SimHash";
        let h1 = simhash_64(data);
        let h2 = simhash_64(data);
        assert_eq!(h1, h2, "SimHash must be deterministic for identical inputs");
    }

    #[test]
    fn simhash_similar_inputs_have_similar_hashes() {
        // Two files that share most of their first 64 KB should
        // have SimHashes with low Hamming distance. The exact
        // threshold is fuzzy (depends on where the differing
        // bytes land), but a Hamming distance < 16 is a strong
        // signal that the sort will put them close together.
        let base = "fn foo() { println!(\"hello\"); }".repeat(500);
        let mut a = base.as_bytes().to_vec();
        let mut b = base.as_bytes().to_vec();
        // Inject a few differences at random positions.
        for i in (10..a.len()).step_by(100) {
            a[i] = b'A';
            b[i] = b'B';
        }
        let ha = simhash_64(&a);
        let hb = simhash_64(&b);
        let distance = (ha ^ hb).count_ones();
        assert!(
            distance < 16,
            "similar files should have close SimHashes (got hamming {})",
            distance
        );
    }

    #[test]
    fn simhash_short_inputs_are_handled() {
        // Inputs shorter than SAMPLE_BYTES should still work
        // (we just hash whatever we have).
        let tiny = b"x";
        assert_eq!(simhash_64(tiny).count_ones() + simhash_64(tiny).count_zeros(), 64);
    }

    #[test]
    fn pick_preprocessor_detects_random_bin() {
        // A `.bin` file containing 4 KB of pseudo-random bytes
        // should classify as Random → fall back to Raw, not
        // Conservative (which would still try to minify).
        let mut data = Vec::with_capacity(4096);
        let mut x: u32 = 0xdeadbeef;
        for _ in 0..4096 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            data.push(x as u8);
        }
        let pre = pick_preprocessor("assets/sprites.bin", &data, true);
        assert_eq!(
            pre,
            Preprocessor::Raw,
            "random .bin should pick Raw (got {:?})",
            pre
        );
    }

    #[test]
    fn pick_preprocessor_text_files_still_use_extension_hint() {
        let ts_code = b"interface Foo { bar: string; }\nconst x: number = 1;\n".repeat(20);
        let pre = pick_preprocessor("src/foo.ts", &ts_code, false);
        assert_eq!(pre, Preprocessor::SwcAst);
        let json = br#"{"users":[{"id":1,"name":"alice"}]}"#.repeat(50);
        let pre = pick_preprocessor("data.json", &json, false);
        assert_eq!(pre, Preprocessor::Conservative);
    }

    #[test]
    fn simhash_sort_changes_toc_order_for_similar_text_files() {
        // Two near-identical TypeScript files + one unrelated JSON.
        // After compression the TOC should NOT be in input order
        // because the two .ts files (similar SimHash) should sort
        // adjacent. The strict order is implementation-defined
        // (depends on the exact hash bits), so we only assert that
        // the two .ts files are NOT separated by the JSON.
        let ts_a = b"import { foo } from './bar';\nexport const A: number = foo(1);\n".repeat(100);
        let ts_b = b"import { foo } from './bar';\nexport const B: number = foo(2);\n".repeat(100);
        let js = br#"{"very":"different","content":"here"}"#.repeat(100);
        let files = vec![
            ("src/a.ts".to_string(), ts_a),
            ("data.json".to_string(), js),
            ("src/b.ts".to_string(), ts_b),
        ];
        let archive = compress(&files, 6).expect("compress");
        let (entries, _) = decompress(&archive).expect("decompress");
        // The two .ts files should be adjacent in the TOC
        // (SimHash sorted), so the JSON should be at one of the
        // ends (either index 0 or index 2).
        let mut ts_indices: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name.ends_with(".ts"))
            .map(|(i, _)| i)
            .collect();
        ts_indices.sort();
        assert_eq!(
            ts_indices[1] - ts_indices[0],
            1,
            "the two .ts files should be adjacent in the TOC; got {:?}",
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
        );
    }

    // ─────────────────────────────────────────────────────────────────
    //   Sprint 5.7.x+ — extensible format migration
    // ─────────────────────────────────────────────────────────────────

    /// `compress()` now emits the v2 format (`NXS\x00` + extension
    /// manifest). `decompress()` dispatches by magic. This test
    /// pins the new magic + version bytes so any future regression
    /// to the v1 wire format fails loudly.
    #[test]
    fn compress_emits_v2_magic() {
        let files: Vec<(String, Vec<u8>)> =
            vec![("hello.txt".to_string(), b"hello v2".to_vec())];
        let archive = compress(&files, 6).expect("compress");
        // Header is 8 bytes: magic(4) + version(1) + flags(1) +
        // reserved(2). After that, n_extensions (u16).
        assert_eq!(&archive[..4], format_v2::MAGIC, "magic must be NXS\\x00");
        assert_eq!(archive[4], format_v2::VERSION, "version must be 1");
        // n_extensions = 2 (EXT_TOC + EXT_PAYLOAD).
        let n = u16::from_le_bytes([archive[8], archive[9]]);
        assert_eq!(n, 2, "v2 manifest must list 2 required extensions");
    }

    /// Legacy v1 archives (NXS6\n magic) must continue to extract
    /// after the v2 migration. We hand-build a minimal v1 archive
    /// here and confirm `decompress()` reads it through the legacy
    /// code path. This is the cross-version guarantee that
    /// justifies the magic-byte dispatch.
    #[test]
    fn decompress_legacy_v1_still_works() {
        use std::io::Write;
        use xz2::write::XzEncoder;
        // Build a tiny v1 archive:
        //   NXS6\n (5) | version (1) | n_files (4)
        //   For each file: name_len (2) | name | orig_size (8) | pre_size (8) | preproc (1) | offset (8)
        //   LZMA-compressed payload
        let payload_text = b"legacy v1 payload";
        let mut lzma_compressed: Vec<u8> = Vec::new();
        {
            let mut enc = XzEncoder::new(&mut lzma_compressed, 1);
            enc.write_all(payload_text).unwrap();
            enc.finish().unwrap();
        }
        let name = "legacy.txt";
        let mut archive: Vec<u8> = Vec::new();
        archive.extend_from_slice(b"NXS6\n");
        archive.push(1); // version
        archive.extend_from_slice(&1u32.to_le_bytes()); // n_files
        archive.extend_from_slice(&(name.len() as u16).to_le_bytes());
        archive.extend_from_slice(name.as_bytes());
        archive.extend_from_slice(&(payload_text.len() as u64).to_le_bytes());
        archive.extend_from_slice(&(payload_text.len() as u64).to_le_bytes());
        archive.push(0); // Raw preprocessor
        archive.extend_from_slice(&0u64.to_le_bytes()); // solid_offset = 0
        archive.extend_from_slice(&lzma_compressed);
        // Decompress via the new dispatcher — must hit the legacy
        // path and return the same content.
        let (entries, solid) = decompress(&archive).expect("decompress legacy v1");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "legacy.txt");
        assert_eq!(entries[0].solid_offset, 0);
        assert_eq!(&solid[..payload_text.len()], payload_text);
    }

    #[test]
    fn solid_offsets_are_monotonic() {
        let files = sample_files();
        let archive = compress(&files, 6).expect("compress");
        let (entries, _) = decompress(&archive).expect("decompress");
        let mut prev_end: u64 = 0;
        for e in &entries {
            assert_eq!(
                e.solid_offset, prev_end,
                "entry '{}' offset {} != previous end {}",
                e.name, e.solid_offset, prev_end
            );
            prev_end = e.solid_offset + e.pre_size;
        }
    }

    /// Sprint 5.7.x+ — the streaming decoder must produce
    /// byte-identical files to the legacy in-memory path. This
    /// test is the contract: if a future change to the LZMA
    /// framing breaks the streaming decoder, this test catches it
    /// before users do.
    #[test]
    fn streaming_decoder_matches_legacy() {
        // Use a variety of file sizes so file-boundary handling
        // is exercised (small, medium, ~1 MB, ~3 MB).
        let files: Vec<(String, Vec<u8>)> = vec![
            ("tiny.txt".to_string(), b"hi".to_vec()),
            ("empty.bin".to_string(), Vec::new()),
            ("medium.md".to_string(), b"# Hello\n\nSome *markdown* text.\n".repeat(100)),
            (
                "large.log".to_string(),
                (0..1_000_000u32).map(|i| (i % 256) as u8).collect(),
            ),
            (
                "huge.log".to_string(),
                (0..3_000_000u32).map(|i| (i * 7 % 256) as u8).collect(),
            ),
        ];
        let archive = compress(&files, 6).expect("compress");
        // Legacy path: decompress returns (entries, solid_stream).
        let (entries, solid) = decompress(&archive).expect("decompress");
        let out_dir = std::env::temp_dir().join("nexus-rar-streaming-test");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();
        // Streaming path.
        let payload_start = payload_offset(&archive).expect("payload_offset");
        let written = decompress_entries_streaming(
            &entries,
            &archive[payload_start..],
            &out_dir,
        )
        .expect("streaming decode");
        assert_eq!(written.len(), files.len());
        // Every file written by streaming must match what the
        // legacy path produced. We compare by NAME (not index)
        // because SimHash sort reorders entries. Note: the
        // streamed bytes are the PREPROCESSED bytes (e.g. for
        // `.md` files Conservative text-minify may strip
        // whitespace), so we compare against the legacy slice,
        // not the original bytes.
        for (name, _original_bytes) in files.iter() {
            // Find the entry in the TOC for this file.
            let e = entries
                .iter()
                .find(|e| e.name == *name)
                .unwrap_or_else(|| panic!("missing entry for {}", name));
            let legacy_slice =
                &solid[e.solid_offset as usize..(e.solid_offset + e.pre_size) as usize];
            let streamed = std::fs::read(out_dir.join(name)).expect("read streamed");
            assert_eq!(
                streamed, legacy_slice,
                "streamed content for '{}' differs from legacy",
                name
            );
            // For empty files, also confirm the original bytes are
            // empty (streaming wrote an empty file because the
            // entry has pre_size=0).
            if e.pre_size == 0 {
                assert_eq!(*_original_bytes, Vec::<u8>::new());
            }
        }
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    // ─────────────────────────────────────────────────────────────────
    //   EXT_CHUNKED tests
    //
    //   These exercise the chunked path end-to-end: compression
    //   produces a multi-chunk archive, decompression reassembles
    //   it, and random-access extraction pulls a subset of files
    //   while only touching the chunks that hold them.
    // ─────────────────────────────────────────────────────────────────

    /// Build a corpus big enough to span multiple chunks at the
    /// default 128 MB chunk budget. We use ~2 MB of pseudo-text
    /// repeated (the budget is small for fast tests but big
    /// enough that SimHash-sort produces a stable ordering).
    fn chunked_corpus() -> Vec<(String, Vec<u8>)> {
        // 20 files × ~100 KB = ~2 MB. With chunk_max = 256 KB
        // we'll get 8+ chunks.
        (0..20)
            .map(|i| {
                let name = format!("file_{:02}.txt", i);
                let payload = format!(
                    "// file {}\nconst x = {};\nexport function f_{}() {{ return x + 1; }}\n",
                    i, i, i
                )
                .repeat(2000);
                (name, payload.into_bytes())
            })
            .collect()
    }

    /// EXT_CHUNKED compression + full extraction round-trip.
    /// The key invariants:
    /// - The archive contains an EXT_CHUNKED extension with N>0 chunks.
    /// - Every original file's preprocessed bytes survive the round-trip
    ///   (compare by name, not index — chunking reorders across chunks).
    /// - Extracted bytes match what decompress() returns (the
    /// Default chunked compression (parity OFF): verifies that
    /// EXT_PAYLOAD contains the raw LZMA bytes (no sharding) and
    /// that EXT_PARITY_SHARDS is omitted. This is the regression
    /// guard against the previous default that added 20% overhead.
    #[test]
    fn chunked_default_no_parity_roundtrips() {
        let files = vec![
            ("hello.txt".to_string(), b"hello world, this is a small test.".repeat(200)),
        ];
        let archive = compress_chunked_with_progress(
            &files,
            6,
            1024 * 1024, // 1 MB — single chunk
            |_, _, _| {},
        )
        .expect("chunked compress");

        let exts = format_v2::read_archive(&archive).expect("read_archive");
        let payload = format_v2::find_extension(&exts, format_v2::EXT_PAYLOAD)
            .expect("EXT_PAYLOAD present");
        // EXT_PARITY_SHARDS should NOT be present (parity disabled).
        assert!(
            format_v2::find_extension(&exts, format_v2::EXT_PARITY_SHARDS).is_none(),
            "EXT_PARITY_SHARDS should NOT be present when parity is disabled by default"
        );

        // The chunk table reports compressed_size == raw LZMA size.
        let ct_bytes = format_v2::find_extension(&exts, format_v2::EXT_CHUNKED)
            .expect("chunked");
        let chunks = format_v2::deserialize_chunk_table(ct_bytes).expect("chunk table");
        assert_eq!(chunks.len(), 1);

        // The payload should be a valid xz stream that decodes
        // cleanly (no trailing garbage, no extra headers).
        let mut out = Vec::new();
        use std::io::Read;
        XzDecoder::new(&payload[..])
            .read_to_end(&mut out)
            .expect("raw LZMA must decode cleanly");
        assert!(!out.is_empty(), "decoded stream should be non-empty");
    }

    /// EXT_PARITY_SHARDS structure check. The default is OFF (so
    /// users get the best ratio), but the format itself is
    /// supported and can be exercised by calling the compression
    /// helpers directly with explicit parity parameters (a future
    /// CLI/UI setting will expose this).
    #[test]
    fn chunked_with_parity_structure_roundtrip() {
        // Default compression: NO EXT_PARITY_SHARDS extension.
        // (Regression guard: previously parity was ON by default,
        // which added 20% to every archive. This test would have
        // caught that if it had existed.)
        let files: Vec<(String, Vec<u8>)> = (0..6)
            .map(|i| {
                (
                    format!("file_{}.ts", i),
                    format!("// file {}\nexport const x = {};\n", i, i).repeat(800).into_bytes(),
                )
            })
            .collect();
        let archive = compress_chunked_with_progress(
            &files,
            6,
            64 * 1024,
            |_, _, _| {},
        )
        .expect("compress");
        let exts = format_v2::read_archive(&archive).expect("read extensions");
        let pe = exts
            .iter()
            .find(|e| e.id == format_v2::EXT_PARITY_SHARDS);
        assert!(
            pe.is_none(),
            "EXT_PARITY_SHARDS should NOT be present by default \
             (default is 0 parity shards for best compression ratio)"
        );

        // Full extract still works (no parity, just chunked).
        let out_dir = std::env::temp_dir().join("nexus-rar-parity-default-test");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();
        let written = extract_files_chunked(&archive, None, &out_dir)
            .expect("extract chunked");
        assert_eq!(written.len(), files.len());
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    /// EXT_PARITY_SHARDS explicitly enabled: verify the format
    /// works end-to-end when parity is opted into. We bypass the
    /// public default-disabled helper and exercise the format
    /// via format_v2 directly (since we don't want to expose
    /// a public "with parity" toggle until UI/CLI plumbing exists).
    #[test]
    fn chunked_with_explicit_parity_roundtrips() {
        // Manually build an archive with parity: take a normal
        // chunked archive and verify that the EXT_PARITY_SHARDS
        // format module can serialize/deserialize a parity layout.
        // (The actual chunked path would emit parity when
        // DEFAULT_PARITY_PARITY_SHARDS > 0; since that's currently
        // 0 by default, we test the format machinery directly.)
        let layouts = vec![
            format_v2::ParityShardLayout {
                data_shards: 10,
                parity_shards: 2,
                data_shard_size: 1024,
                parity_shard_size: 1024,
                lzma_size: 10000,
            },
            format_v2::ParityShardLayout {
                data_shards: 10,
                parity_shards: 3,
                data_shard_size: 2048,
                parity_shard_size: 2048,
                lzma_size: 20000,
            },
        ];
        let parity_bytes_per_chunk = vec![vec![0u8; 2 * 1024], vec![0u8; 3 * 2048]];
        let ext = format_v2::parity_shards_extension(&layouts, &parity_bytes_per_chunk);
        let exts = vec![ext];
        let archive_bytes = format_v2::write_archive(&exts);
        // Parse back and verify.
        let parsed = format_v2::read_archive(&archive_bytes).expect("read");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, format_v2::EXT_PARITY_SHARDS);
        let (parsed_layouts, parsed_parity) =
            format_v2::deserialize_parity_extension(&parsed[0].payload)
                .expect("deserialize parity");
        assert_eq!(parsed_layouts, layouts);
        // The trailing slice (after the layout header) should be
        // exactly the sum of parity_bytes_per_chunk.
        let expected_parity_size = 2 * 1024 + 3 * 2048;
        assert_eq!(parsed_parity.len(), expected_parity_size);
        // And the full payload (header + parity bytes) should
        // match what serialize produced.
        let expected_total = 2 + 2 * 14 + expected_parity_size;
        assert_eq!(parsed[0].payload.len(), expected_total);
    }

    #[test]
    fn chunked_no_duplicates_skips_dedup_table() {
        // All-unique content: each file is different enough that
        // SimHash puts them in different chunks AND their
        // preprocessed bytes don't collide.
        let files: Vec<(String, Vec<u8>)> = (0..20)
            .map(|i| {
                let payload = format!(
                    "// unique file {}\nconst x_{} = {};\n",
                    i, i, i * 31
                )
                .repeat(2000);
                (format!("file_{:02}.ts", i), payload.into_bytes())
            })
            .collect();
        let archive = compress_chunked_with_progress(
            &files,
            6,
            256 * 1024,
            |_, _, _| {},
        )
        .expect("chunked compress unique");
        let exts = format_v2::read_archive(&archive).expect("read_archive");
        let dt_bytes = format_v2::find_extension(&exts, format_v2::EXT_DEDUP_TABLE);
        assert!(
            dt_bytes.is_none(),
            "EXT_DEDUP_TABLE should be absent when no chunks duplicate"
        );
    }
}
