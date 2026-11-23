//! NXS v2 / NX6R native lossless packer, unpacker, verifier & inspector.
//!
//! Wire-compatible with `hextapp-pipeline/storage/nx6r.py` and
//! `hextapp-android/NexusEngine.kt`:
//!   - Header: `NXS\x00` (4B) | version=1 (1B) | flags=0 (1B) | reserved=0 (2B) | n_extensions (u16 LE)
//!   - `0x0001 EXT_TOC`
//!   - `0x0060 EXT_APK_MANIFEST` (APK-aware DEFLATE unpack + META-INF strip)
//!   - `0x0061 EXT_SHA256`
//!   - `0x0062 EXT_CHUNKS` (parallel XZ blocks decoded via rayon)
//!   - `0x0002 EXT_PAYLOAD`

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use sha2::{Digest, Sha256};
use xz2::read::XzDecoder;
use xz2::stream::{Check, Filters, LzmaOptions, Stream};
use xz2::write::XzEncoder;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::ram;

pub const MAGIC: &[u8; 4] = b"NXS\x00";
pub const VERSION: u8 = 1;

pub const EXT_TOC: u16 = 0x0001;
pub const EXT_PAYLOAD: u16 = 0x0002;
pub const EXT_APK_MANIFEST: u16 = 0x0060;
pub const EXT_SHA256: u16 = 0x0061;
pub const EXT_CHUNKS: u16 = 0x0062;

#[derive(Debug, Clone)]
pub struct TocEntry {
    pub name: String,
    pub original_size: u64,
    pub pre_size: u64,
    pub preprocessor: u8,
    pub chunk_id: u8,
    pub offset: u64,
}

#[derive(Debug, Clone)]
pub struct ApkEntryMeta {
    pub entry_name: String,
    pub method: u16,
    pub crc32: u32,
    pub uncompressed_size: u64,
}

#[derive(Debug, Clone)]
pub struct ApkManifest {
    pub apk_name: String,
    pub entries: Vec<ApkEntryMeta>,
}

#[derive(Debug, Clone)]
pub struct ChunkMeta {
    pub chunk_id: u8,
    pub payload_offset: u64,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
}

#[derive(Debug, Clone)]
pub struct Nx6rInfo {
    pub version: u8,
    pub n_extensions: u16,
    pub file_size: u64,
    pub raw_total_bytes: u64,
    pub toc_entries: Vec<TocEntry>,
    pub sha256_map: HashMap<String, [u8; 32]>,
    pub apk_manifests: Vec<ApkManifest>,
    pub chunks: Vec<ChunkMeta>,
    pub payload_offset: u64,
    pub payload_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UnitCategory {
    MediaFast,
    BcjX86,
    BcjArm,
    General,
}

#[derive(Debug, Clone)]
enum UnitSource {
    Disk(PathBuf),
    ZipEntry { zip_path: PathBuf, entry_name: String },
}

#[derive(Debug, Clone)]
struct PackUnit {
    name: String,
    source: UnitSource,
    size: u64,
    category: UnitCategory,
}

fn is_uncompressed_apk_ext(ext: &str) -> bool {
    matches!(
        ext,
        ".png"
            | ".jpg"
            | ".jpeg"
            | ".webp"
            | ".gif"
            | ".mp3"
            | ".ogg"
            | ".aac"
            | ".mp4"
            | ".m4a"
            | ".wav"
            | ".so"
            | ".arsc"
    )
}

fn is_media_fast_ext(ext: &str) -> bool {
    matches!(
        ext,
        ".png"
            | ".jpg"
            | ".jpeg"
            | ".webp"
            | ".gif"
            | ".mp3"
            | ".ogg"
            | ".aac"
            | ".mp4"
            | ".m4a"
            | ".wav"
    )
}

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| format!(".{}", s.to_ascii_lowercase()))
        .unwrap_or_default()
}

fn stem_of(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_known_compressible_ext(ext: &str) -> bool {
    matches!(
        ext,
        ".dex" | ".so" | ".arsc" | ".xml" | ".json" | ".dll" | ".exe" | ".txt" | ".js" | ".css" | ".html" | ".obb"
    )
}

fn shannon_entropy(sample: &[u8]) -> f64 {
    if sample.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in sample {
        counts[b as usize] += 1;
    }
    let n = sample.len() as f64;
    let mut ent = 0.0f64;
    for &c in &counts {
        if c > 0 {
            let p = (c as f64) / n;
            ent -= p * p.log2();
        }
    }
    ent
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackProfile {
    Archive,
    Mobile,
    Fast,
    Ultra,
}

impl PackProfile {
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "mobile" => Self::Mobile,
            "fast" => Self::Fast,
            "ultra" => Self::Ultra,
            _ => Self::Archive,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChunkStat {
    pub chunk_id: usize,
    pub role: String,
    pub entries: usize,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    pub preset: u32,
}

#[derive(Debug, Clone)]
pub struct PackStats {
    pub profile: PackProfile,
    pub preset: u32,
    pub raw_total_bytes: u64,
    pub compressible_bytes: u64,
    pub incompressible_bytes: u64,
    pub group_a_comp: u64,
    pub group_b_comp: u64,
    pub out_bytes: u64,
    pub sha256_hex: String,
    pub chunks: Vec<ChunkStat>,
    pub top_entries: Vec<(String, u64)>,
}

fn classify_unit_with_sample(name: &str, sample: &[u8], size: u64) -> UnitCategory {
    if size < 128 * 1024 {
        return UnitCategory::General;
    }
    let low = name.to_ascii_lowercase();
    let ext = ext_of(&low);
    if is_media_fast_ext(&ext) || ext == ".usm" || ext == ".awb" {
        UnitCategory::MediaFast
    } else if ext == ".exe" || ext == ".dll" || (ext == ".so" && low.contains("/x86")) {
        UnitCategory::BcjX86
    } else if ext == ".so" && low.contains("/armeabi") {
        UnitCategory::BcjArm
    } else if !sample.is_empty() && !is_known_compressible_ext(&ext) && shannon_entropy(sample) >= 7.85 {
        UnitCategory::MediaFast
    } else {
        UnitCategory::General
    }
}

fn is_old_apk_signature(entry_name: &str) -> bool {
    let upper = entry_name.to_ascii_uppercase();
    upper.starts_with("META-INF/")
        && (upper.ends_with(".SF")
            || upper.ends_with(".RSA")
            || upper.ends_with(".DSA")
            || upper.ends_with(".EC")
            || upper == "META-INF/MANIFEST.MF")
}

fn is_zip_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    File::open(path)
        .ok()
        .map(|f| ZipArchive::new(BufReader::new(f)).is_ok())
        .unwrap_or(false)
}

fn collect_dir_files(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let p = entry.path();
        if p.is_dir() {
            collect_dir_files(&p, base, out)?;
        } else if p.is_file() {
            let rel = p
                .strip_prefix(base)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((p, rel));
        }
    }
    Ok(())
}

fn safe_join(base: &Path, rel: &str) -> io::Result<PathBuf> {
    let clean = rel.replace('\\', "/");
    let clean = clean.trim_start_matches('/');
    let mut target = base.to_path_buf();
    for comp in Path::new(clean).components() {
        match comp {
            std::path::Component::Normal(c) => target.push(c),
            std::path::Component::CurDir => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsafe path in archive: {rel}"),
                ))
            }
        }
    }
    Ok(target)
}

fn copy_unit_to_writer<W: Write>(src: &UnitSource, out: &mut W) -> io::Result<u64> {
    match src {
        UnitSource::Disk(path) => {
            let mut f = BufReader::with_capacity(256 * 1024, File::open(path)?);
            io::copy(&mut f, out)
        }
        UnitSource::ZipEntry {
            zip_path,
            entry_name,
        } => {
            let f = BufReader::with_capacity(256 * 1024, File::open(zip_path)?);
            let mut za = ZipArchive::new(f).map_err(io::Error::other)?;
            let mut zf = za.by_name(entry_name).map_err(io::Error::other)?;
            io::copy(&mut zf, out)
        }
    }
}

fn compress_xz_stream(cat: UnitCategory, preset: u32, bucket: &[PackUnit]) -> io::Result<(Vec<u8>, Vec<TocEntry>, u64)> {
    let eff_preset = if cat == UnitCategory::MediaFast {
        1
    } else {
        ram::clamp_preset_for_ram(preset, ram::available_memory_mb())
    };
    let opts = LzmaOptions::new_preset(eff_preset).map_err(io::Error::other)?;
    let mut filters = Filters::new();
    match cat {
        UnitCategory::BcjX86 => {
            filters.x86();
        }
        UnitCategory::BcjArm => {
            filters.arm();
        }
        UnitCategory::MediaFast | UnitCategory::General => {}
    }
    filters.lzma2(&opts);
    let stream = Stream::new_stream_encoder(&filters, Check::Crc64).map_err(io::Error::other)?;
    let mut encoder = XzEncoder::new_stream(Vec::new(), stream);

    let mut local_toc = Vec::with_capacity(bucket.len());
    let mut offset = 0u64;
    for u in bucket {
        let written = copy_unit_to_writer(&u.source, &mut encoder)?;
        local_toc.push(TocEntry {
            name: u.name.clone(),
            original_size: written,
            pre_size: written,
            preprocessor: 0,
            chunk_id: 0, // filled by caller
            offset,
        });
        offset += written;
    }
    let compressed = encoder.finish()?;
    Ok((compressed, local_toc, offset))
}

pub fn pack_nx6r(
    source_path: &Path,
    output_path: &Path,
    preset: u32,
    unpack_apks: bool,
    threads: Option<usize>,
) -> io::Result<(u64, u64, String)> {
    let stats = pack_nx6r_with_profile(source_path, output_path, preset, unpack_apks, threads, PackProfile::Archive)?;
    Ok((stats.raw_total_bytes, stats.out_bytes, stats.sha256_hex))
}

pub fn pack_nx6r_with_profile(
    source_path: &Path,
    output_path: &Path,
    preset: u32,
    unpack_apks: bool,
    threads: Option<usize>,
    profile: PackProfile,
) -> io::Result<PackStats> {
    if !source_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("source not found: {}", source_path.display()),
        ));
    }

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_bundle_dir = output_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".nx6r_bundle_{}", std::process::id()));
    if tmp_bundle_dir.exists() {
        let _ = fs::remove_dir_all(&tmp_bundle_dir);
    }

    let result = (|| -> io::Result<PackStats> {
        let mut raw_files: Vec<(PathBuf, String)> = Vec::new();
        if source_path.is_file() {
            let fname = source_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string();
            raw_files.push((source_path.to_path_buf(), fname));
        } else {
            collect_dir_files(source_path, source_path, &mut raw_files)?;
        }

        let mut expanded_files: Vec<(PathBuf, String)> = Vec::new();
        for (disk_path, rel_name) in raw_files {
            let ext = ext_of(&rel_name);
            if (ext == ".xapk" || ext == ".apks") && is_zip_file(&disk_path) {
                fs::create_dir_all(&tmp_bundle_dir)?;
                let mut za = ZipArchive::new(BufReader::new(File::open(&disk_path)?)).map_err(io::Error::other)?;
                for i in 0..za.len() {
                    let mut zf = za.by_index(i).map_err(io::Error::other)?;
                    if zf.is_dir() {
                        continue;
                    }
                    let inner_name = zf.name().replace('\\', "/");
                    let dest = safe_join(&tmp_bundle_dir, &inner_name)?;
                    if let Some(p) = dest.parent() {
                        fs::create_dir_all(p)?;
                    }
                    let mut out = BufWriter::new(File::create(&dest)?);
                    io::copy(&mut zf, &mut out)?;
                    expanded_files.push((dest, inner_name));
                }
            } else {
                expanded_files.push((disk_path, rel_name));
            }
        }

        if expanded_files.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no files to pack"));
        }

        let mut units: Vec<PackUnit> = Vec::new();
        let mut apk_manifests: Vec<ApkManifest> = Vec::new();
        let mut sha_list: Vec<(String, [u8; 32])> = Vec::new();
        let mut total_raw_bytes = 0u64;
        let mut buf = vec![0u8; 256 * 1024];

        for (disk_path, rel_name) in expanded_files {
            let ext = ext_of(&rel_name);
            if unpack_apks && ext == ".apk" && is_zip_file(&disk_path) {
                let mut za = ZipArchive::new(BufReader::new(File::open(&disk_path)?)).map_err(io::Error::other)?;
                let mut apk_items = Vec::new();
                for i in 0..za.len() {
                    let mut zf = za.by_index(i).map_err(io::Error::other)?;
                    if zf.is_dir() {
                        continue;
                    }
                    let entry_name = zf.name().to_string();
                    if is_old_apk_signature(&entry_name) {
                        continue;
                    }
                    let mut hasher = Sha256::new();
                    let mut entry_size = 0u64;
                    let mut sample_buf = Vec::new();
                    loop {
                        let n = zf.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        if sample_buf.len() < 65536 {
                            let take = (65536 - sample_buf.len()).min(n);
                            sample_buf.extend_from_slice(&buf[..take]);
                        }
                        hasher.update(&buf[..n]);
                        entry_size += n as u64;
                    }
                    let digest: [u8; 32] = hasher.finalize().into();
                    let vname = format!("__nx6r_apk__/{rel_name}/{entry_name}");
                    sha_list.push((vname.clone(), digest));

                    let e_ext = ext_of(&entry_name);
                    let method = if zf.compression() == CompressionMethod::Stored || is_uncompressed_apk_ext(&e_ext) {
                        0u16
                    } else {
                        8u16
                    };
                    apk_items.push(ApkEntryMeta {
                        entry_name: entry_name.clone(),
                        method,
                        crc32: zf.crc32(),
                        uncompressed_size: entry_size,
                    });
                    units.push(PackUnit {
                        category: classify_unit_with_sample(&vname, &sample_buf, entry_size),
                        name: vname,
                        source: UnitSource::ZipEntry {
                            zip_path: disk_path.clone(),
                            entry_name,
                        },
                        size: entry_size,
                    });
                    total_raw_bytes += entry_size;
                }
                apk_manifests.push(ApkManifest {
                    apk_name: rel_name,
                    entries: apk_items,
                });
            } else {
                let mut hasher = Sha256::new();
                let mut f = BufReader::with_capacity(256 * 1024, File::open(&disk_path)?);
                let mut file_size = 0u64;
                let mut sample_buf = Vec::new();
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    if sample_buf.len() < 65536 {
                        let take = (65536 - sample_buf.len()).min(n);
                        sample_buf.extend_from_slice(&buf[..take]);
                    }
                    hasher.update(&buf[..n]);
                    file_size += n as u64;
                }
                let digest: [u8; 32] = hasher.finalize().into();
                sha_list.push((rel_name.clone(), digest));
                units.push(PackUnit {
                    category: classify_unit_with_sample(&rel_name, &sample_buf, file_size),
                    name: rel_name,
                    source: UnitSource::Disk(disk_path),
                    size: file_size,
                });
                total_raw_bytes += file_size;
            }
        }

        let mut top_entries: Vec<(String, u64)> = units.iter().map(|u| (u.name.clone(), u.size)).collect();
        top_entries.sort_by(|a, b| b.1.cmp(&a.1));
        top_entries.truncate(10);

        let (mut group_a, mut group_b): (Vec<PackUnit>, Vec<PackUnit>) = units
            .into_iter()
            .partition(|u| u.category != UnitCategory::MediaFast);

        // ABI Affinity + Solid Ordering: keep all ABI variants of the same .so & classes*.dex together
        group_a.sort_by(|a, b| {
            ext_of(&a.name)
                .cmp(&ext_of(&b.name))
                .then_with(|| stem_of(&a.name).cmp(&stem_of(&b.name)))
                .then_with(|| a.name.cmp(&b.name))
        });
        group_b.sort_by(|a, b| ext_of(&a.name).cmp(&ext_of(&b.name)).then_with(|| a.name.cmp(&b.name)));

        let compressible_bytes: u64 = group_a.iter().map(|u| u.size).sum();
        let incompressible_bytes: u64 = group_b.iter().map(|u| u.size).sum();

        let avail_mb = ram::available_memory_mb().unwrap_or(4096);
        let (mut eff_preset, solid_single_cap) = match profile {
            PackProfile::Mobile => (preset.min(7), 192 * 1024 * 1024),
            PackProfile::Fast => (preset.min(3), 96 * 1024 * 1024),
            PackProfile::Ultra => (if avail_mb >= 1536 { 9 } else { preset.max(7) }, 384 * 1024 * 1024),
            PackProfile::Archive => {
                let mut p = preset;
                if compressible_bytes >= 32 * 1024 * 1024 && avail_mb >= 2048 && preset >= 6 {
                    p = p.max(8);
                } else if compressible_bytes >= 16 * 1024 * 1024 && avail_mb >= 1024 && preset >= 5 {
                    p = p.max(7);
                }
                (p, 224 * 1024 * 1024)
            }
        };
        eff_preset = ram::clamp_preset_for_ram(eff_preset, Some(avail_mb));

        let n_a = if let Some(req_t) = threads {
            ram::clamp_threads_for_ram(req_t, eff_preset).min(group_a.len().max(1))
        } else if compressible_bytes <= solid_single_cap || group_a.len() < 4 {
            1
        } else {
            let target = ((compressible_bytes + (96 * 1024 * 1024 - 1)) / (96 * 1024 * 1024)) as usize;
            let max_w = ram::clamp_threads_for_ram(rayon::current_num_threads().clamp(2, 6), eff_preset);
            target.clamp(2, max_w.min(group_a.len()))
        };

        let mut toc_entries: Vec<TocEntry> = Vec::with_capacity(group_a.len() + group_b.len());
        let mut chunk_metas: Vec<ChunkMeta> = Vec::new();
        let mut chunk_stats: Vec<ChunkStat> = Vec::new();
        let payload_bytes: Vec<u8>;

        let mut jobs: Vec<(UnitCategory, Vec<PackUnit>)> = Vec::new();
        if !group_a.is_empty() {
            if n_a <= 1 {
                jobs.push((UnitCategory::General, group_a));
            } else {
                let mut sub: Vec<Vec<PackUnit>> = vec![Vec::new(); n_a];
                let mut sub_sizes = vec![0u64; n_a];
                for u in group_a {
                    let idx = sub_sizes
                        .iter()
                        .enumerate()
                        .min_by_key(|&(_, s)| s)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    sub_sizes[idx] += u.size;
                    sub[idx].push(u);
                }
                for b in sub {
                    if !b.is_empty() {
                        jobs.push((UnitCategory::General, b));
                    }
                }
            }
        }
        if !group_b.is_empty() {
            jobs.push((UnitCategory::MediaFast, group_b));
        }

        if jobs.len() <= 1 && threads != Some(1) {
            let (cat, only_units) = jobs.into_iter().next().unwrap_or((UnitCategory::General, Vec::new()));
            let n_entries = only_units.len();
            let (compressed, mut local_toc, uncomp) = compress_xz_stream(cat, eff_preset, &only_units)?;
            for e in &mut local_toc {
                e.chunk_id = 0xFF;
            }
            toc_entries.extend(local_toc);
            chunk_stats.push(ChunkStat {
                chunk_id: 0,
                role: if cat == UnitCategory::MediaFast { "media_fast" } else { "general" }.to_string(),
                entries: n_entries,
                raw_bytes: uncomp,
                compressed_bytes: compressed.len() as u64,
                preset: if cat == UnitCategory::MediaFast { 1 } else { eff_preset },
            });
            payload_bytes = compressed;
        } else {
            let job_roles: Vec<UnitCategory> = jobs.iter().map(|(c, _)| *c).collect();
            let compressed_jobs: Vec<io::Result<(usize, Vec<u8>, Vec<TocEntry>, u64)>> = jobs
                .into_par_iter()
                .enumerate()
                .map(|(cid, (cat, bucket))| {
                    let (bytes, mut ltoc, uncomp) = compress_xz_stream(cat, eff_preset, &bucket)?;
                    for e in &mut ltoc {
                        e.chunk_id = cid as u8;
                    }
                    Ok((cid, bytes, ltoc, uncomp))
                })
                .collect();

            let mut merged = Vec::new();
            let mut payload_off = 0u64;
            let mut ok_jobs = Vec::with_capacity(compressed_jobs.len());
            for item in compressed_jobs {
                ok_jobs.push(item?);
            }
            ok_jobs.sort_by_key(|j| j.0);
            for (cid, bytes, ltoc, uncomp) in ok_jobs {
                let comp_sz = bytes.len() as u64;
                let role_cat = job_roles[cid];
                chunk_stats.push(ChunkStat {
                    chunk_id: cid,
                    role: if role_cat == UnitCategory::MediaFast { "media_fast" } else { "general" }.to_string(),
                    entries: ltoc.len(),
                    raw_bytes: uncomp,
                    compressed_bytes: comp_sz,
                    preset: if role_cat == UnitCategory::MediaFast { 1 } else { eff_preset },
                });
                chunk_metas.push(ChunkMeta {
                    chunk_id: cid as u8,
                    payload_offset: payload_off,
                    compressed_size: comp_sz,
                    uncompressed_size: uncomp,
                });
                payload_off += comp_sz;
                toc_entries.extend(ltoc);
                merged.extend_from_slice(&bytes);
            }
            payload_bytes = merged;
        }

        let toc_bytes = serialize_toc(&toc_entries);
        let sha_bytes = serialize_sha256(&sha_list);
        let mut exts: Vec<(u16, Vec<u8>)> = vec![(EXT_TOC, toc_bytes), (EXT_SHA256, sha_bytes)];
        if !apk_manifests.is_empty() {
            exts.push((EXT_APK_MANIFEST, serialize_apk_manifests(&apk_manifests)));
        }
        if !chunk_metas.is_empty() {
            exts.push((EXT_CHUNKS, serialize_chunks(&chunk_metas)));
        }

        let n_extensions = (exts.len() + 1) as u16;
        let payload_len = payload_bytes.len() as u64;
        let payload_hdr_len = if payload_len <= u32::MAX as u64 {
            payload_len as u32
        } else {
            0u32
        };

        let mut archive_hasher = Sha256::new();
        let mut out_f = BufWriter::new(File::create(output_path)?);
        let mut header = Vec::with_capacity(10);
        header.extend_from_slice(MAGIC);
        header.push(VERSION);
        header.push(0);
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&n_extensions.to_le_bytes());
        out_f.write_all(&header)?;
        archive_hasher.update(&header);

        for (eid, epayload) in exts {
            let mut ehdr = [0u8; 6];
            ehdr[0..2].copy_from_slice(&eid.to_le_bytes());
            ehdr[2..6].copy_from_slice(&(epayload.len() as u32).to_le_bytes());
            out_f.write_all(&ehdr)?;
            out_f.write_all(&epayload)?;
            archive_hasher.update(&ehdr);
            archive_hasher.update(&epayload);
        }

        let mut phdr = [0u8; 6];
        phdr[0..2].copy_from_slice(&EXT_PAYLOAD.to_le_bytes());
        phdr[2..6].copy_from_slice(&payload_hdr_len.to_le_bytes());
        out_f.write_all(&phdr)?;
        out_f.write_all(&payload_bytes)?;
        out_f.flush()?;
        archive_hasher.update(&phdr);
        archive_hasher.update(&payload_bytes);

        let out_size = fs::metadata(output_path)?.len();
        let hex = archive_hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        let group_a_comp: u64 = chunk_stats.iter().filter(|c| c.role != "media_fast").map(|c| c.compressed_bytes).sum();
        let group_b_comp: u64 = chunk_stats.iter().filter(|c| c.role == "media_fast").map(|c| c.compressed_bytes).sum();
        Ok(PackStats {
            profile,
            preset: eff_preset,
            raw_total_bytes: total_raw_bytes,
            compressible_bytes,
            incompressible_bytes,
            group_a_comp,
            group_b_comp,
            out_bytes: out_size,
            sha256_hex: hex,
            chunks: chunk_stats,
            top_entries,
        })
    })();

    if tmp_bundle_dir.exists() {
        let _ = fs::remove_dir_all(&tmp_bundle_dir);
    }
    result
}

pub fn read_info(archive_path: &Path) -> io::Result<Nx6rInfo> {
    let file_size = fs::metadata(archive_path)?.len();
    let mut f = BufReader::new(File::open(archive_path)?);
    let mut hdr = [0u8; 10];
    f.read_exact(&mut hdr)?;
    if &hdr[0..4] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid NXS v2 magic"));
    }
    let version = hdr[4];
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported NXS v2 version: {version}"),
        ));
    }
    let n_extensions = u16::from_le_bytes([hdr[8], hdr[9]]);

    let mut toc_entries = Vec::new();
    let mut sha256_map = HashMap::new();
    let mut apk_manifests = Vec::new();
    let mut chunks = Vec::new();
    let mut payload_offset = None;
    let mut payload_size = 0u64;

    for _ in 0..n_extensions {
        let mut ehdr = [0u8; 6];
        f.read_exact(&mut ehdr)?;
        let eid = u16::from_le_bytes([ehdr[0], ehdr[1]]);
        let esize = u32::from_le_bytes([ehdr[2], ehdr[3], ehdr[4], ehdr[5]]) as u64;
        match eid {
            EXT_TOC => {
                let mut data = vec![0u8; esize as usize];
                f.read_exact(&mut data)?;
                toc_entries = parse_toc(&data)?;
            }
            EXT_SHA256 => {
                let mut data = vec![0u8; esize as usize];
                f.read_exact(&mut data)?;
                sha256_map = parse_sha256(&data)?;
            }
            EXT_APK_MANIFEST => {
                let mut data = vec![0u8; esize as usize];
                f.read_exact(&mut data)?;
                apk_manifests = parse_apk_manifests(&data)?;
            }
            EXT_CHUNKS => {
                let mut data = vec![0u8; esize as usize];
                f.read_exact(&mut data)?;
                chunks = parse_chunks(&data)?;
            }
            EXT_PAYLOAD => {
                let pos = f.stream_position()?;
                payload_offset = Some(pos);
                payload_size = if esize == 0 { file_size.saturating_sub(pos) } else { esize };
                break;
            }
            _ => {
                f.seek(SeekFrom::Current(esize as i64))?;
            }
        }
    }

    let payload_offset = payload_offset.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "NXS v2 archive missing EXT_PAYLOAD")
    })?;
    let raw_total_bytes = toc_entries.iter().map(|e| e.pre_size).sum();

    Ok(Nx6rInfo {
        version,
        n_extensions,
        file_size,
        raw_total_bytes,
        toc_entries,
        sha256_map,
        apk_manifests,
        chunks,
        payload_offset,
        payload_size,
    })
}

fn process_stream_subset<R: Read>(
    mut xz: XzDecoder<R>,
    subset: &[TocEntry],
    sha_map: &HashMap<String, [u8; 32]>,
    output_dir: Option<&Path>,
) -> io::Result<usize> {
    let mut sorted = subset.to_vec();
    sorted.sort_by_key(|e| e.offset);

    let mut stream_pos = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    let mut count = 0usize;

    for entry in sorted {
        if entry.offset > stream_pos {
            let mut to_skip = entry.offset - stream_pos;
            let max_buf = buf.len() as u64;
            while to_skip > 0 {
                let to_read = to_skip.min(max_buf) as usize;
                let n = xz.read(&mut buf[..to_read])?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF skipping"));
                }
                to_skip -= n as u64;
                stream_pos += n as u64;
            }
        }

        let mut writer = if let Some(dir) = output_dir {
            let out_f = safe_join(dir, &entry.name)?;
            if let Some(p) = out_f.parent() {
                fs::create_dir_all(p)?;
            }
            Some((out_f.clone(), BufWriter::new(File::create(&out_f)?)))
        } else {
            None
        };

        let mut hasher = Sha256::new();
        let mut rem = entry.pre_size;
        while rem > 0 {
            let to_read = rem.min(buf.len() as u64) as usize;
            let n = xz.read(&mut buf[..to_read])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("unexpected EOF reading {}", entry.name),
                ));
            }
            hasher.update(&buf[..n]);
            if let Some((_, ref mut w)) = writer {
                w.write_all(&buf[..n])?;
            }
            rem -= n as u64;
            stream_pos += n as u64;
        }
        if let Some((_, ref mut w)) = writer {
            w.flush()?;
        }

        let digest: [u8; 32] = hasher.finalize().into();
        if let Some(expected) = sha_map.get(&entry.name) {
            if &digest != expected {
                if let Some((path, _)) = writer {
                    let _ = fs::remove_file(path);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("SHA-256 mismatch in {}", entry.name),
                ));
            }
        }
        count += 1;
    }
    Ok(count)
}

pub fn verify_nx6r(archive_path: &Path) -> io::Result<usize> {
    let info = read_info(archive_path)?;
    if info.chunks.is_empty() {
        let mut f = File::open(archive_path)?;
        f.seek(SeekFrom::Start(info.payload_offset))?;
        let xz = XzDecoder::new(BufReader::new(f));
        process_stream_subset(xz, &info.toc_entries, &info.sha256_map, None)
    } else {
        let mut by_chunk: HashMap<u8, Vec<TocEntry>> = HashMap::new();
        for e in &info.toc_entries {
            by_chunk.entry(e.chunk_id).or_default().push(e.clone());
        }
        let results: Vec<io::Result<usize>> = info
            .chunks
            .par_iter()
            .map(|cm| {
                let subset = by_chunk.get(&cm.chunk_id).cloned().unwrap_or_default();
                if subset.is_empty() {
                    return Ok(0);
                }
                let mut f = File::open(archive_path)?;
                f.seek(SeekFrom::Start(info.payload_offset + cm.payload_offset))?;
                let bounded = f.take(cm.compressed_size);
                let xz = XzDecoder::new(BufReader::new(bounded));
                process_stream_subset(xz, &subset, &info.sha256_map, None)
            })
            .collect();
        let mut total = 0usize;
        for r in results {
            total += r?;
        }
        Ok(total)
    }
}

pub fn unpack_nx6r(archive_path: &Path, output_dir: &Path) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(output_dir)?;
    let info = read_info(archive_path)?;

    if info.chunks.is_empty() {
        let mut f = File::open(archive_path)?;
        f.seek(SeekFrom::Start(info.payload_offset))?;
        let xz = XzDecoder::new(BufReader::new(f));
        process_stream_subset(xz, &info.toc_entries, &info.sha256_map, Some(output_dir))?;
    } else {
        let mut by_chunk: HashMap<u8, Vec<TocEntry>> = HashMap::new();
        for e in &info.toc_entries {
            by_chunk.entry(e.chunk_id).or_default().push(e.clone());
        }
        let results: Vec<io::Result<usize>> = info
            .chunks
            .par_iter()
            .map(|cm| {
                let subset = by_chunk.get(&cm.chunk_id).cloned().unwrap_or_default();
                if subset.is_empty() {
                    return Ok(0);
                }
                let mut f = File::open(archive_path)?;
                f.seek(SeekFrom::Start(info.payload_offset + cm.payload_offset))?;
                let bounded = f.take(cm.compressed_size);
                let xz = XzDecoder::new(BufReader::new(bounded));
                process_stream_subset(xz, &subset, &info.sha256_map, Some(output_dir))
            })
            .collect();
        for r in results {
            r?;
        }
    }

    let mut written = Vec::new();
    for e in &info.toc_entries {
        if !e.name.starts_with("__nx6r_apk__/") {
            written.push(safe_join(output_dir, &e.name)?);
        }
    }

    if !info.apk_manifests.is_empty() {
        let vroot = output_dir.join("__nx6r_apk__");
        for manifest in &info.apk_manifests {
            let apk_out = safe_join(output_dir, &manifest.apk_name)?;
            if let Some(p) = apk_out.parent() {
                fs::create_dir_all(p)?;
            }
            let src_dir = vroot.join(&manifest.apk_name);
            let mut zw = ZipWriter::new(BufWriter::new(File::create(&apk_out)?));
            for item in &manifest.entries {
                let src_file = safe_join(&src_dir, &item.entry_name)?;
                let method = if item.method == 0 {
                    CompressionMethod::Stored
                } else {
                    CompressionMethod::Deflated
                };
                let opts = SimpleFileOptions::default().compression_method(method);
                zw.start_file(item.entry_name.as_str(), opts).map_err(io::Error::other)?;
                let mut rf = BufReader::new(File::open(&src_file)?);
                io::copy(&mut rf, &mut zw)?;
            }
            zw.finish().map_err(io::Error::other)?;
            written.push(apk_out);
        }
        let _ = fs::remove_dir_all(&vroot);
    }

    Ok(written)
}

fn serialize_toc(entries: &[TocEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        let nb = e.name.as_bytes();
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(nb);
        out.extend_from_slice(&e.original_size.to_le_bytes());
        out.extend_from_slice(&e.pre_size.to_le_bytes());
        out.push(e.preprocessor);
        out.push(e.chunk_id);
        out.extend_from_slice(&e.offset.to_le_bytes());
    }
    out
}

fn serialize_sha256(hashes: &[(String, [u8; 32])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(hashes.len() as u32).to_le_bytes());
    for (p, d) in hashes {
        let pb = p.as_bytes();
        out.extend_from_slice(&(pb.len() as u16).to_le_bytes());
        out.extend_from_slice(pb);
        out.extend_from_slice(d);
    }
    out
}

fn serialize_apk_manifests(manifests: &[ApkManifest]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(manifests.len() as u32).to_le_bytes());
    for m in manifests {
        let ab = m.apk_name.as_bytes();
        out.extend_from_slice(&(ab.len() as u16).to_le_bytes());
        out.extend_from_slice(&(m.entries.len() as u32).to_le_bytes());
        out.extend_from_slice(ab);
        for item in &m.entries {
            let eb = item.entry_name.as_bytes();
            out.extend_from_slice(&(eb.len() as u16).to_le_bytes());
            out.extend_from_slice(eb);
            out.extend_from_slice(&item.method.to_le_bytes());
            out.extend_from_slice(&item.crc32.to_le_bytes());
            out.extend_from_slice(&item.uncompressed_size.to_le_bytes());
        }
    }
    out
}

fn serialize_chunks(chunks: &[ChunkMeta]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for c in chunks {
        out.push(c.chunk_id);
        out.extend_from_slice(&c.payload_offset.to_le_bytes());
        out.extend_from_slice(&c.compressed_size.to_le_bytes());
        out.extend_from_slice(&c.uncompressed_size.to_le_bytes());
    }
    out
}

fn parse_toc(data: &[u8]) -> io::Result<Vec<TocEntry>> {
    let mut pos = 0usize;
    let n = read_u32(data, &mut pos)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let nlen = read_u16(data, &mut pos)? as usize;
        let name = read_str(data, &mut pos, nlen)?;
        let original_size = read_u64(data, &mut pos)?;
        let pre_size = read_u64(data, &mut pos)?;
        let preprocessor = read_u8(data, &mut pos)?;
        let chunk_id = read_u8(data, &mut pos)?;
        let offset = read_u64(data, &mut pos)?;
        out.push(TocEntry {
            name,
            original_size,
            pre_size,
            preprocessor,
            chunk_id,
            offset,
        });
    }
    Ok(out)
}

fn parse_sha256(data: &[u8]) -> io::Result<HashMap<String, [u8; 32]>> {
    let mut pos = 0usize;
    let n = read_u32(data, &mut pos)? as usize;
    let mut map = HashMap::with_capacity(n);
    for _ in 0..n {
        let plen = read_u16(data, &mut pos)? as usize;
        let path = read_str(data, &mut pos, plen)?;
        if pos + 32 > data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated sha256"));
        }
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&data[pos..pos + 32]);
        pos += 32;
        map.insert(path, digest);
    }
    Ok(map)
}

fn parse_apk_manifests(data: &[u8]) -> io::Result<Vec<ApkManifest>> {
    let mut pos = 0usize;
    let n = read_u32(data, &mut pos)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let alen = read_u16(data, &mut pos)? as usize;
        let n_items = read_u32(data, &mut pos)? as usize;
        let apk_name = read_str(data, &mut pos, alen)?;
        let mut entries = Vec::with_capacity(n_items);
        for _ in 0..n_items {
            let elen = read_u16(data, &mut pos)? as usize;
            let entry_name = read_str(data, &mut pos, elen)?;
            let method = read_u16(data, &mut pos)?;
            let crc32 = read_u32(data, &mut pos)?;
            let uncompressed_size = read_u64(data, &mut pos)?;
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

fn parse_chunks(data: &[u8]) -> io::Result<Vec<ChunkMeta>> {
    let mut pos = 0usize;
    let n = read_u32(data, &mut pos)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let chunk_id = read_u8(data, &mut pos)?;
        let payload_offset = read_u64(data, &mut pos)?;
        let compressed_size = read_u64(data, &mut pos)?;
        let uncompressed_size = read_u64(data, &mut pos)?;
        out.push(ChunkMeta {
            chunk_id,
            payload_offset,
            compressed_size,
            uncompressed_size,
        });
    }
    Ok(out)
}

fn read_u8(d: &[u8], p: &mut usize) -> io::Result<u8> {
    if *p + 1 > d.len() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof u8"));
    }
    let v = d[*p];
    *p += 1;
    Ok(v)
}

fn read_u16(d: &[u8], p: &mut usize) -> io::Result<u16> {
    if *p + 2 > d.len() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof u16"));
    }
    let v = u16::from_le_bytes([d[*p], d[*p + 1]]);
    *p += 2;
    Ok(v)
}

fn read_u32(d: &[u8], p: &mut usize) -> io::Result<u32> {
    if *p + 4 > d.len() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof u32"));
    }
    let v = u32::from_le_bytes([d[*p], d[*p + 1], d[*p + 2], d[*p + 3]]);
    *p += 4;
    Ok(v)
}

fn read_u64(d: &[u8], p: &mut usize) -> io::Result<u64> {
    if *p + 8 > d.len() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof u64"));
    }
    let v = u64::from_le_bytes([
        d[*p],
        d[*p + 1],
        d[*p + 2],
        d[*p + 3],
        d[*p + 4],
        d[*p + 5],
        d[*p + 6],
        d[*p + 7],
    ]);
    *p += 8;
    Ok(v)
}

fn read_str(d: &[u8], p: &mut usize, len: usize) -> io::Result<String> {
    if *p + len > d.len() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof str"));
    }
    let s = String::from_utf8(d[*p..*p + len].to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    *p += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn nx6r_apk_parallel_roundtrip_and_verify() {
        let dir = TempDir::new().unwrap();
        let apk = dir.path().join("demo.apk");
        {
            let f = File::create(&apk).unwrap();
            let mut zw = ZipWriter::new(f);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zw.start_file("AndroidManifest.xml", opts).unwrap();
            zw.write_all(&b"<manifest/>".repeat(50)).unwrap();
            zw.start_file("classes.dex", opts).unwrap();
            zw.write_all(&b"dexbytes".repeat(100)).unwrap();
            zw.finish().unwrap();
        }

        let out_nxs = dir.path().join("demo.nxs");
        let (raw, comp, _sha) = pack_nx6r(&apk, &out_nxs, 1, true, Some(2)).unwrap();
        assert!(raw > 0 && comp > 0);

        let info = read_info(&out_nxs).unwrap();
        assert_eq!(info.apk_manifests.len(), 1);
        assert_eq!(info.chunks.len(), 2);

        let verified = verify_nx6r(&out_nxs).unwrap();
        assert_eq!(verified, 2);

        let unp_dir = dir.path().join("unpacked");
        let written = unpack_nx6r(&out_nxs, &unp_dir).unwrap();
        assert!(written.iter().any(|p| p.file_name().and_then(|n| n.to_str()) == Some("demo.apk")));

        // Corrupt payload and verify fails
        let mut bytes = fs::read(&out_nxs).unwrap();
        let last = bytes.len() - 5;
        bytes[last] ^= 0xFF;
        fs::write(&out_nxs, &bytes).unwrap();
        assert!(verify_nx6r(&out_nxs).is_err());
    }
}
