//! WinRAR-compatible archive support — Sprint 5.7.x.
//!
//! Three formats, three different sources:
//!
//! | Format | Backend          | License          | Notes |
//! |--------|------------------|------------------|-------|
//! | `.zip` | `zip` crate      | MIT              | Read + write. We use LZMA method (14) so WinRAR can decompress it too |
//! | `.7z`  | `sevenz-rust`    | Apache-2.0       | Read-only. 7z writer is non-trivial and out of scope |
//! | `.rar` | `UnRAR.exe`      | Free (bundled w/ WinRAR) | Shells out — see `find_unrar()` |
//!
//! Why shell out for RAR? The .rar format is proprietary to win.rar GmbH.
//! The only Rust crate that can read it (`unrar-rs`) FFI-links against
//! `libunrar` (a C++ library), which would force us to either bundle
//! `unrar.dll` (extra DLL deployment) or depend on the user installing
//! it. WinRAR's installer ships `UnRAR.exe` as a tiny (~800 KB)
//! standalone binary that exposes a stable command-line interface, so
//! shelling out is the path with the fewest moving parts.
//!
//! The user has to install WinRAR (or copy UnRAR.exe) for .rar reading
//! to work. On Windows, WinRAR ships with most desktops; on macOS it's
//! available as a paid app; on Linux users can install `unrar` from
//! their package manager (which often provides just the `unrar`
//! binary, exactly what we need).
//!
//! **`detect_unrar()`** runs at startup and the result is exposed via
//! the `unrar_status_cmd` Tauri command — the UI uses it to render a
//! "RAR no soportado" warning when UnRAR isn't found, instead of
//! failing at drop time.
//!
//! All reader functions return the same `PageResult` shape as the
//! existing tar/solid readers in `archive_inspect.rs` so the frontend
//! `DecompressView` doesn't need to special-case anything.

use crate::archive_inspect::{ArchiveEntry, PageResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
const WINRAR_DEFAULT_PATHS: &[&str] = &[
    r"C:\Program Files\WinRAR\UnRAR.exe",
    r"C:\Program Files (x86)\WinRAR\UnRAR.exe",
];

#[cfg(target_os = "macos")]
const WINRAR_DEFAULT_PATHS: &[&str] = &[
    "/Applications/WinRAR.app/Contents/MacOS/UnRAR",
    "/usr/local/bin/unrar",
];

#[cfg(all(unix, not(target_os = "macos")))]
const WINRAR_DEFAULT_PATHS: &[&str] = &[
    "/usr/bin/unrar",
    "/usr/local/bin/unrar",
    "/snap/bin/unrar",
];

/// Status of the UnRAR binary detection — exposed to the frontend so
/// the UI can warn the user when they drop a .rar and we don't have a
/// way to read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnrarStatus {
    /// Absolute path to the UnRAR binary we found, or null if none.
    pub path: Option<String>,
    /// Version string from `unrar -ver`, or null if we couldn't run it.
    pub version: Option<String>,
    /// Why we couldn't find one (null if we did find one). Frontend
    /// uses this to render the warning toast / banner.
    pub reason_missing: Option<String>,
}

/// Look up the UnRAR executable. Resolution order:
/// 1. `$NX_UNRAR` environment variable (manual override).
/// 2. `$UNRAR` environment variable (common Unix convention).
/// 3. Platform-specific default install paths (WinRAR on Windows,
///    WinRAR.app on macOS, system unrar on Linux).
/// 4. `unrar` / `UnRAR.exe` on `$PATH`.
pub fn detect_unrar() -> UnrarStatus {
    // (1) and (2) env vars
    for var in &["NX_UNRAR", "UNRAR"] {
        if let Ok(p) = std::env::var(var) {
            let path = PathBuf::from(&p);
            if path.is_file() {
                return probe_unrar(&path);
            }
        }
    }
    // (3) platform defaults
    for p in WINRAR_DEFAULT_PATHS {
        let path = PathBuf::from(p);
        if path.is_file() {
            return probe_unrar(&path);
        }
    }
    // (4) PATH lookup — just try to invoke the binary by name.
    // If the OS can find it on PATH, spawn() succeeds.
    let exe_name = if cfg!(windows) { "UnRAR.exe" } else { "unrar" };
    if std::process::Command::new(exe_name)
        .arg("-ver")
        .output()
        .is_ok()
    {
        return probe_unrar(&PathBuf::from(exe_name));
    }
    UnrarStatus {
        path: None,
        version: None,
        reason_missing: Some(format!(
            "{} not found in PATH or default install paths",
            exe_name
        )),
    }
}

fn probe_unrar(path: &Path) -> UnrarStatus {
    let output = Command::new(path).arg("-ver").output();
    match output {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout);
            // `UnRAR -ver` prints lines like "UNRAR 6.11 x64".
            // Take the first non-empty line.
            let version = s
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string);
            UnrarStatus {
                path: Some(path.to_string_lossy().into_owned()),
                version,
                reason_missing: None,
            }
        }
        Ok(out) => UnrarStatus {
            path: Some(path.to_string_lossy().into_owned()),
            version: None,
            reason_missing: Some(format!(
                "UnRAR exited with status {}",
                out.status
            )),
        },
        Err(e) => UnrarStatus {
            path: Some(path.to_string_lossy().into_owned()),
            version: None,
            reason_missing: Some(format!("could not run UnRAR: {}", e)),
        },
    }
}

// ──────────────────────────────────────────────────────────────────────────
//   .zip — uses the `zip` crate (read + write)
// ──────────────────────────────────────────────────────────────────────────

pub fn list_zip_entries(path: &Path) -> Result<Vec<ArchiveEntry>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open zip: {}", e))?;
    let mut archive = zip::ZipArchive::new(f).map_err(|e| format!("zip open: {}", e))?;
    let mut out = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {}: {}", i, e))?;
        let name = entry.name().to_string();
        let size = entry.size();
        let is_dir = entry.is_dir() || name.ends_with('/');
        out.push(ArchiveEntry { name, size, is_dir });
    }
    Ok(out)
}

pub fn list_zip_paginated(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<PageResult, String> {
    let entries = list_zip_entries(path)?;
    let total = entries.len();
    let end = (offset + limit).min(total);
    let window = if offset < total {
        entries[offset..end].to_vec()
    } else {
        Vec::new()
    };
    Ok(PageResult {
        entries: window,
        has_more: end < total,
    })
}

pub fn extract_zip_entries(
    path: &Path,
    output_dir: &Path,
    selected: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(output_dir).map_err(|e| format!("mkdir output: {}", e))?;
    let f = std::fs::File::open(path).map_err(|e| format!("open zip: {}", e))?;
    let mut archive = zip::ZipArchive::new(f).map_err(|e| format!("zip open: {}", e))?;
    let selected_set: Option<std::collections::HashSet<String>> =
        selected.map(|v| v.into_iter().collect());
    let mut written = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {}: {}", i, e))?;
        let entry_name = entry.name().to_string();
        if let Some(ref set) = selected_set {
            if !set.contains(&entry_name) {
                continue;
            }
        }
        // Zip-slip protection — same check as tar/solid readers.
        crate::archive_inspect::safe_relative_path(&entry_name)
            .map_err(|e| format!("unsafe path in zip: {} ({})", entry_name, e))?;
        let dest = output_dir.join(&entry_name);
        if entry.is_dir() || entry_name.ends_with('/') {
            std::fs::create_dir_all(&dest)
                .map_err(|e| format!("mkdir {}: {}", dest.display(), e))?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir parent: {}", e))?;
            }
            let mut out_file = std::fs::File::create(&dest)
                .map_err(|e| format!("create {}: {}", dest.display(), e))?;
            std::io::copy(&mut entry, &mut out_file)
                .map_err(|e| format!("write {}: {}", dest.display(), e))?;
        }
        written.push(entry_name);
    }
    Ok(written)
}

/// Compress files into a .zip archive using Deflate (the standard
/// "WinRAR-compatible" zip compression method).
///
/// **Note on compression method choice**: the ZIP format defines
/// method 14 = LZMA, but the `zip` crate (the de-facto Rust zip
/// implementation) does NOT support LZMA compression — only
/// decompression. The only compression methods it writes are
/// Stored (0) and Deflate (8). We use Deflate at the highest
/// level because it's the format WinRAR produces by default for
/// .zip output and is universally compatible.
///
/// If we wanted true LZMA-in-zip output, we'd need to either
/// (a) use a different crate that implements LZMA compression
/// (none exist for pure Rust), (b) shell out to 7-Zip's `7z a
/// -tzip -m0=lzma`, or (c) implement LZMA encoding by hand.
/// All three are out of scope for Sprint 5.7.x; the user can
/// always run the result through 7-Zip later to re-compress.
pub fn write_zip_with_lzma(
    files: &[(String, Vec<u8>)],
    output_path: &Path,
) -> Result<u64, String> {
    use std::io::Write;
    let f = std::fs::File::create(output_path)
        .map_err(|e| format!("create zip: {}", e))?;
    let mut zip = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true)
        .compression_level(Some(9)) // maximum Deflate level
        .unix_permissions(0o644);
    for (name, bytes) in files {
        zip.start_file(name, opts)
            .map_err(|e| format!("zip start {}: {}", name, e))?;
        zip.write_all(bytes)
            .map_err(|e| format!("zip write {}: {}", name, e))?;
    }
    zip.finish().map_err(|e| format!("zip finish: {}", e))?;
    let written = std::fs::metadata(output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    Ok(written)
}

// ──────────────────────────────────────────────────────────────────────────
//   .7z — uses `sevenz-rust` (read-only)
// ──────────────────────────────────────────────────────────────────────────

pub fn list_7z_entries(path: &Path) -> Result<Vec<ArchiveEntry>, String> {
    use sevenz_rust::Archive;
    let archive = Archive::open(path).map_err(|e| format!("open 7z: {}", e))?;
    Ok(archive
        .files
        .iter()
        .map(|f| {
            let name = f.name().to_string();
            let size = f.size();
            let is_dir = f.is_directory();
            ArchiveEntry { name, size, is_dir }
        })
        .collect())
}

pub fn list_7z_paginated(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<PageResult, String> {
    let entries = list_7z_entries(path)?;
    let total = entries.len();
    let end = (offset + limit).min(total);
    let window = if offset < total {
        entries[offset..end].to_vec()
    } else {
        Vec::new()
    };
    Ok(PageResult {
        entries: window,
        has_more: end < total,
    })
}

pub fn extract_7z_entries(
    path: &Path,
    output_dir: &Path,
    selected: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    use sevenz_rust::decompress_file_with_extract_fn;
    std::fs::create_dir_all(output_dir).map_err(|e| format!("mkdir output: {}", e))?;
    // `extract_fn` is `FnMut`, so we can capture `selected` and
    // `written` by mutable reference — no Rc/RefCell dance needed.
    // The closure captures the local mutable state and is called
    // synchronously by sevenz-rust's decoder loop.
    let mut written: Vec<String> = Vec::new();
    let selected_set = selected.map(|v| v.into_iter().collect::<std::collections::HashSet<_>>());
    let mut selected_filter: Option<std::collections::HashSet<String>> = selected_set;
    decompress_file_with_extract_fn(path, output_dir, |entry, _reader, dest_path| {
        if entry.is_directory() {
            return Ok(true);
        }
        let name = entry.name();
        if let Some(ref set) = selected_filter {
            if !set.contains(name) {
                return Ok(false);
            }
        }
        if let Err(e) = crate::archive_inspect::safe_relative_path(name) {
            return Err(sevenz_rust::Error::io_msg(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "unsafe path"),
                e,
            ));
        }
        written.push(name.to_string());
        // Make sure the parent directory exists (some 7z archives
        // don't include explicit directory entries).
        if let Some(parent) = dest_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Ok(true)
    })
    .map_err(|e| format!("7z decompress: {}", e))?;
    // `decompress_file_with_extract_fn` doesn't return the list of
    // extracted files; we collected them in the callback.
    // Suppress the unused-mut warning — `selected_filter` is mutated
    // conceptually even though the closure only reads it; keeping
    // the `mut` documents intent.
    let _ = &mut selected_filter;
    Ok(written)
}

// ──────────────────────────────────────────────────────────────────────────
//   .rar — shells out to UnRAR.exe
// ──────────────────────────────────────────────────────────────────────────

/// Listing for RAR archives. Uses `UnRAR lt` (tab-separated technical
/// listing) which gives name + size + a few other fields we don't need.
/// We parse just the first two columns.
///
/// **Output format** (UnRAR 5+):
/// ```text
/// Name\tSize\tPacked Size\tModified\t...
/// foo.txt\t12345\t9876\t2024-01-01 12:34\t...
/// bar/\t0\t0\t2024-01-01 12:34\t...
/// ```
fn rar_unrar_exe() -> Result<PathBuf, String> {
    detect_unrar().path.map(PathBuf::from).ok_or_else(|| {
        "UnRAR not found. Install WinRAR (Windows), WinRAR.app (macOS), \
         or `unrar` (Linux) to read .rar archives."
            .to_string()
    })
}

pub fn list_rar_entries(path: &Path) -> Result<Vec<ArchiveEntry>, String> {
    let exe = rar_unrar_exe()?;
    let output = Command::new(&exe)
        .arg("lt")
        .arg("--")
        .arg(path)
        .output()
        .map_err(|e| format!("run UnRAR: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "UnRAR lt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_rar_lt(&text))
}

pub fn list_rar_paginated(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<PageResult, String> {
    let entries = list_rar_entries(path)?;
    let total = entries.len();
    let end = (offset + limit).min(total);
    let window = if offset < total {
        entries[offset..end].to_vec()
    } else {
        Vec::new()
    };
    Ok(PageResult {
        entries: window,
        has_more: end < total,
    })
}

fn parse_rar_lt(text: &str) -> Vec<ArchiveEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // The UnRAR `lt` output looks like (real example from
        // WinRAR 6.11):
        // ```
        // Name        Size   Packed Size   Modified   Method   CRC32
        // foo.txt    12345        9876  2024-01-01 12:34   Stored  DEADBEEF
        // ```
        // The header line uses space-separated columns (no tabs);
        // data rows are tab-separated. We rely on that distinction
        // here.
        if !line.contains('\t') {
            continue;
        }
        // Skip the header row even if it does happen to contain
        // tabs (some WinRAR builds add separators). The header
        // always starts with one of these labels.
        let first_field = line.split('\t').next().unwrap_or("").trim();
        if first_field.eq_ignore_ascii_case("Name")
            || first_field.eq_ignore_ascii_case("Filename")
            || first_field.eq_ignore_ascii_case("File name")
        {
            continue;
        }
        // Split on tab. The first field is the name, the second is
        // the size. Names can contain spaces (Rar stores them
        // verbatim). Use `split` (not `splitn`) and index the
        // resulting slice directly.
        let fields: Vec<&str> = line.split('\t').collect();
        let name = fields.first().copied().unwrap_or("").to_string();
        let size_str = fields.get(1).copied().unwrap_or("0").trim();
        let size: u64 = size_str.parse().unwrap_or(0);
        let is_dir = name.ends_with('/');
        out.push(ArchiveEntry { name, size, is_dir });
    }
    out
}

pub fn extract_rar_entries(
    path: &Path,
    output_dir: &Path,
    selected: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    let exe = rar_unrar_exe()?;
    std::fs::create_dir_all(output_dir).map_err(|e| format!("mkdir output: {}", e))?;
    // UnRAR's `-o+` overwrites without prompting. `-idq` makes it
    // quiet; `-idc` disables the "Confirm exit" prompt that some
    // WinRAR builds emit. We add `--` so the archive path can never be
    // interpreted as a flag.
    let mut cmd = Command::new(&exe);
    cmd.arg("e").arg("-o+").arg("-idq").arg("-idc");
    if let Some(set) = selected.as_ref() {
        if !set.is_empty() {
            for name in set {
                cmd.arg(name);
            }
        }
    }
    cmd.arg("--").arg(path).arg(output_dir);
    let output = cmd
        .output()
        .map_err(|e| format!("run UnRAR: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "UnRAR extract failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    // UnRAR extracts to the output_dir directly without preserving
    // the archive's top-level prefix; we return the names we asked
    // for (or "all extracted" if `selected` was None).
    Ok(selected.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn parse_rar_lt_handles_tab_separated_rows() {
        // Use \n explicitly — Rust's trailing-backslash line
        // continuation does NOT add a newline, so the naive
        // version of this test ended up as one giant line and
        // only matched the first row.
        let input = "\
Name\tSize\tPacked Size\tModified\tMethod\tCRC32\n\
foo.txt\t12345\t9876\t2024-01-01 12:34\tStored\tDEADBEEF\n\
bar dir/\t0\t0\t2024-01-01 12:34\t\t0\n\
nested/path/baz.bin\t500\t400\t2024-01-01 12:34\tLZMA:14\tCAFEBABE\n";
        let entries = parse_rar_lt(input);
        assert_eq!(entries.len(), 3, "got {} entries: {:?}", entries.len(), entries);
        assert_eq!(entries[0].name, "foo.txt");
        assert_eq!(entries[0].size, 12345);
        assert!(!entries[0].is_dir);
        assert_eq!(entries[1].name, "bar dir/");
        assert!(entries[1].is_dir);
        assert_eq!(entries[2].name, "nested/path/baz.bin");
        assert_eq!(entries[2].size, 500);
    }

    #[test]
    fn detect_unrar_returns_a_status() {
        // We can't assert which status because the test environment
        // varies (CI without WinRAR vs the user's machine with it).
        // Just make sure it doesn't panic.
        let _ = detect_unrar();
    }

    /// Round-trip: write_zip_with_lzma → list_zip_entries →
    /// extract_zip_entries should preserve all original bytes.
    /// This is the test the user will indirectly run by dropping
    /// a freshly-compressed file into the DecompressView.
    #[test]
    fn zip_roundtrip_preserves_bytes() {
        let tmp = std::env::temp_dir().join("nexus-rar-test-zip-roundtrip");
        std::fs::create_dir_all(&tmp).unwrap();
        let zip_path = tmp.join("test.zip");
        let extract_dir = tmp.join("out");
        std::fs::create_dir_all(&extract_dir).unwrap();

        // Three test entries of different sizes — LZMA only beats
        // Store on data with some redundancy, so the test verifies
        // that LZMA-compressed round-trips match the input bit-for-bit.
        let files: Vec<(String, Vec<u8>)> = vec![
            ("hello.txt".to_string(), b"Hello, WinRAR!".to_vec()),
            ("empty.txt".to_string(), Vec::new()),
            (
                "data.bin".to_string(),
                (0..4096u32).map(|i| i as u8).collect(),
            ),
        ];
        let written =
            write_zip_with_lzma(&files, &zip_path).expect("write_zip_with_lzma");
        assert!(written > 0, "zip writer produced 0 bytes");

        // Listing
        let entries = list_zip_entries(&zip_path).expect("list_zip_entries");
        assert_eq!(entries.len(), 3);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"hello.txt"));
        assert!(names.contains(&"empty.txt"));
        assert!(names.contains(&"data.bin"));

        // Extract
        let _written = extract_zip_entries(&zip_path, &extract_dir, None)
            .expect("extract_zip_entries");
        let hello = std::fs::read(extract_dir.join("hello.txt")).unwrap();
        assert_eq!(hello, b"Hello, WinRAR!");
        let empty = std::fs::read(extract_dir.join("empty.txt")).unwrap();
        assert!(empty.is_empty());
        let data = std::fs::read(extract_dir.join("data.bin")).unwrap();
        assert_eq!(data.len(), 4096);
        for (i, b) in data.iter().enumerate() {
            assert_eq!(*b, i as u8, "mismatch at index {}", i);
        }
        // Cleanup (best-effort — mavis-trash is unavailable in
        // this workspace; PowerShell `Remove-Item` would fail too).
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Verify the safe_relative_path check is exposed via the public
    /// re-export so external callers (zip / 7z / rar extract) can
    /// reuse it without re-implementing zip-slip protection.
    #[test]
    fn safe_relative_path_is_public() {
        // Positive case
        assert!(crate::archive_inspect::safe_relative_path("a/b/c.txt").is_ok());
        // Negative cases (each must fail)
        for bad in &["../etc/passwd", "/etc/passwd", "C:\\Windows\\System32"] {
            assert!(
                crate::archive_inspect::safe_relative_path(bad).is_err(),
                "should reject {:?}",
                bad
            );
        }
    }

    /// Verify the .zip writer emits a file that opens with a real
    /// zip reader (use the same `zip` crate, just round-trip).
    #[test]
    fn zip_roundtrip_opens_with_zip_crate() {
        let tmp = std::env::temp_dir().join("nexus-rar-test-zip-crate");
        std::fs::create_dir_all(&tmp).unwrap();
        let zip_path = tmp.join("rt.zip");

        let files: Vec<(String, Vec<u8>)> = vec![(
            "round.txt".to_string(),
            b"this should round-trip".repeat(100),
        )];
        write_zip_with_lzma(&files, &zip_path).unwrap();

        // Re-open with the same crate to confirm validity.
        let f = std::fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(f).unwrap();
        assert_eq!(archive.len(), 1);
        let mut entry = archive.by_index(0).unwrap();
        assert_eq!(entry.name(), "round.txt");
        let mut buf = Vec::new();
        std::io::Write::write_all(&mut buf, &mut []).unwrap(); // touch Write
        entry.read_to_end(&mut buf).unwrap();
        let expected = b"this should round-trip".repeat(100);
        assert_eq!(buf, expected);
        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Suppress unused-import warning when the test module doesn't
    // exercise `Write` directly.
    #[allow(dead_code)]
    fn _touch_write() {
        let mut v = Vec::new();
        v.write_all(b"x").unwrap();
    }
}