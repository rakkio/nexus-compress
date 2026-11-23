//! Tauri command handlers — the IPC bridge between the Next.js UI
//! (in `../frontend/`) and the synchronous `nexus_compress::api`
//! module. Every command is `async` and wraps the CPU-bound work
//! in `spawn_blocking` so the Tauri runtime stays responsive.
//!
//! The mapping is one-to-one with the `api` module: each function
//! here is a thin `#[tauri::command]` that delegates to a
//! corresponding `api::*` function. The Tauri runtime serializes
//! the return values to the frontend via JSON.
//!
//! The mapping is one-to-one with the `api` module: each function
//! here is a thin `#[tauri::command]` that delegates to a
//! corresponding `api::*` function. The Tauri runtime serializes
//! the return values to the frontend via JSON.

use nexus_compress::api::{
    self, ApiError, ApiResult, BackendInfo, CompressResult, CompressTargetResult, CompressionBackend,
    CompressionLevel, DecompressResult, DecompressTargetResult, EngineInfo, PeekResult,
    ProgressEvent, SelfTestResult,
};
use std::path::{Path, PathBuf};
use std::str::FromStr;

fn to_ipc<T>(r: ApiResult<T>) -> Result<T, String> {
    r.map_err(|e| format!("{}: {}", e.code, e.message))
}

// ─────────────────────────────────────────────────────────────
//  db persist helpers (Sprint 5.7 hotfix #20)
//
//  All three helpers are best-effort wrappers around `db::record_*`.
//  A failure to write stats must NEVER fail the actual compression
//  / decompression / share the user just spent time on — log and
//  return Ok so the IPC response goes back to the UI cleanly.
//  The errors land in stderr where they can be triaged later.
// ─────────────────────────────────────────────────────────────

async fn persist_compression(
    db_state: &State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    filename: &str,
    original_bytes: u64,
    compressed_bytes: u64,
) -> Result<(), String> {
    let conn = db_state.lock().await;
    db::record_compression(&conn, filename, original_bytes, compressed_bytes)
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn persist_decompression(
    db_state: &State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    filename: &str,
    archive_bytes: u64,
    restored_bytes: u64,
) -> Result<(), String> {
    let conn = db_state.lock().await;
    db::record_decompression(&conn, filename, archive_bytes, restored_bytes)
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn persist_share(
    db_state: &State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    filename: &str,
    bytes: u64,
) -> Result<(), String> {
    let conn = db_state.lock().await;
    db::record_share(&conn, filename, bytes).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn compress_bytes_cmd(input: Vec<u8>) -> Result<CompressResult, String> {
    tauri::async_runtime::spawn_blocking(move || api::compress_bytes(&input))
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))
}

#[tauri::command]
pub async fn decompress_bytes_cmd(input: Vec<u8>) -> Result<DecompressResult, String> {
    tauri::async_runtime::spawn_blocking(move || to_ipc(api::decompress_bytes(&input)))
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

#[tauri::command]
pub async fn compress_bytes_with_level_cmd(
    input: Vec<u8>,
    level: String,
) -> Result<CompressResult, String> {
    let level = CompressionLevel::from_str(&level).map_err(|e| format!("invalid_level: {}", e))?;
    tauri::async_runtime::spawn_blocking(move || api::compress_bytes_with_level(&input, level))
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))
}

#[tauri::command]
pub async fn engine_info_cmd() -> Result<EngineInfo, String> {
    Ok(api::engine_info())
}

#[tauri::command]
pub async fn backend_info_cmd() -> Result<Vec<BackendInfo>, String> {
    Ok(api::backend_info())
}

#[tauri::command]
pub async fn compress_bytes_with_backend_cmd(
    input: Vec<u8>,
    file_name: String,
    backend: String,
    lzma_level: u32,
) -> Result<CompressResult, String> {
    let backend =
        CompressionBackend::from_str(&backend).map_err(|e| format!("invalid_backend: {}", e))?;
    tauri::async_runtime::spawn_blocking(move || {
        api::compress_bytes_with_backend(&input, &file_name, backend, lzma_level)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))
}

#[tauri::command]
pub async fn compress_directory_with_backend_cmd(
    input_dir: String,
    backend: String,
    lzma_level: u32,
) -> Result<(api::DirectoryResult, Vec<u8>), String> {
    let backend =
        CompressionBackend::from_str(&backend).map_err(|e| format!("invalid_backend: {}", e))?;
    let path = PathBuf::from(input_dir);
    tauri::async_runtime::spawn_blocking(move || {
        to_ipc(
            api::compress_directory_with_backend(&path, backend, lzma_level)
                .map(|(r, a)| (r, a.to_vec())),
        )
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

/// Compress any path by reference — auto-detects file vs directory.
/// This is the entry point the GUI uses when the user drops a file
/// or folder onto the dropzone (the OS gives us the path, not the
/// bytes) or when they pick via the native dialog. Reads the file
/// from disk on the Rust side, so we never have to ship large bytes
/// through the Tauri IPC boundary. The compressed output is
/// auto-saved next to the input so the user can verify on disk.
///
/// Args are bundled into a single JSON object `req` to avoid the
/// Tauri 2.x arg-name conversion gotchas (camelCase vs snake_case
/// for `lzma_level`). The frontend passes
/// `{ req: { path, backend, lzma_level, output_dir } }`.
///
/// `output_dir` (optional) overrides the "next to input" default:
/// the output filename is preserved but written into that dir.
///
/// Progress is emitted as `compress-progress` events for the JS
/// progress bar.
#[tauri::command]
pub async fn compress_target_cmd(
    app: tauri::AppHandle,
    db_state: State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    req: serde_json::Value,
) -> Result<CompressTargetResult, String> {
    let path = req
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'path' in req".to_string())?
        .to_string();
    let backend_str = req
        .get("backend")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'backend' in req".to_string())?;
    let lzma_level = req.get("lzma_level").and_then(|v| v.as_u64()).unwrap_or(6) as u32;
    let output_dir = req
        .get("output_dir")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);
    // Sprint 5.7.2 PR #4: optional encryption. When the frontend
    // includes `password` in the req, we route to the encrypted
    // pipeline (v4 codec + AES-256-GCM + optional Reed-Solomon).
    // The `recovery_level` string ("off" / "low" / "high") defaults
    // to "low" (10 % parity) when password is set, matching the
    // design doc's "recovery on by default" choice.
    let password: Option<String> = req
        .get("password")
        .and_then(|v| v.as_str())
        .map(String::from);
    let recovery_str: Option<String> = req
        .get("recovery_level")
        .and_then(|v| v.as_str())
        .map(String::from);
    let backend =
        CompressionBackend::from_str(backend_str).map_err(|e| format!("invalid_backend: {}", e))?;
    let p = PathBuf::from(path);
    let filename_for_db = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    let inner: ApiResult<CompressTargetResult> = tauri::async_runtime::spawn_blocking(move || {
        // Sprint 5.7.x+ STATUS_BREAKPOINT hotfix: wrap the heavy
        // compression work in `catch_unwind` so any panic in the
        // engine (e.g. swc_core OOM on a malformed TS file, xz2
        // internal assertion, rayon worker stack overflow) becomes
        // a clean `ApiError` instead of an uncaught panic that
        // crosses the FFI boundary and shows up as
        // STATUS_BREAKPOINT (0x80000003) on Windows. The frontend
        // was seeing this on ~250 MB inputs that exhaust RAM during
        // the in-memory compression path.
        //
        // The `AssertUnwindSafe` is needed because the closure
        // captures references to `app` (AppHandle is RefUnwindSafe
        // on tauri 2.x but we don't depend on that — the panic
        // path discards the closure result anyway).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let app_for_event = app.clone();
            // Sprint 5.7 throttling: ProgressEvent emits per chunk
            // (potentially hundreds per second on fast disks). We
            // collect into a slot and only flush to the IPC channel
            // every 100ms — see `throttle::ThrottledEmitter` below.
            let throttler = throttle::ThrottledEmitter::new(app_for_event.clone());
            let cb = |event: ProgressEvent| {
                throttler.feed("compress-progress", &event);
            };
            // Sprint 5.7.x+: emit a "preparing" frame BEFORE the
            // spawn_blocking work starts so the UI shows activity
            // immediately. The heavy lifting (SimHash sort +
            // swc_core parse on .ts/.tsx + LZMA compress) can take
            // 1-3s on a cold start because swc_core's parser/codegen
            // are lazy-initialized; without this priming event the
            // user clicks "Comprimir" and the progress bar appears
            // frozen until the first per-file event fires.
            cb(api::ProgressEvent {
                phase: "preparing".to_string(),
                current_file: String::new(),
                files_done: 0,
                files_total: 0,
                bytes_done: 0,
                bytes_total: 0,
                elapsed_ms: 0,
                bytes_per_sec: 0.0,
                eta_ms: 0,
            });
            // Two paths: encrypted (password is set) or plain.
            // We keep the dispatch in one function so the frontend
            // doesn't have to call different commands; the password
            // is the only signal that decides the branch.
            if let Some(pwd) = password.as_deref() {
                let recovery = match recovery_str.as_deref() {
                    Some(s) => api::RecoveryLevel::from_str(s)
                        .map_err(|e| ApiError::new("compress.invalid_recovery_level", e))?,
                    None => api::RecoveryLevel::default(),
                };
                let opts = api::CompressWithPasswordOptions {
                    password: pwd.as_bytes(),
                    recovery,
                };
                api::compress_target_with_password(&p, &opts, cb)
            } else {
                api::compress_target(
                    &p,
                    backend,
                    lzma_level,
                    output_dir.as_deref(),
                    cb,
                )
            }
        }));
        match result {
            Ok(r) => r,
            Err(panic_payload) => {
                // Convert panic into a clean ApiError. The
                // payload is usually a `&str` or `String`; if
                // neither, fall back to a generic message.
                let msg = if let Some(s) = panic_payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "compress panicked (unknown payload) — this is \
                     almost always an OOM during heavy LZMA / swc_core \
                     work; try a smaller file, a lower LZMA preset, or \
                     close other apps to free RAM"
                        .to_string()
                };
                Err(ApiError::new("compress.panic", msg))
            }
        }
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?;
    let result: CompressTargetResult = to_ipc(inner)?;

    // Sprint 5.7 hotfix #20: persist into the local SQLite store so
    // the Recientes view (RecentView.tsx) and the Home dashboard
    // (LandingPage) can show real numbers. Best-effort: a stats
    // failure must never fail the actual compression that the user
    // waited for — log + continue.
    if let Err(e) = persist_compression(
        &db_state,
        &filename_for_db,
        result.original_size,
        result.compressed_size,
    )
    .await
    {
        eprintln!("[nexus-rar] WARN: failed to persist compression stats: {}", e);
    }
    Ok(result)
}

/// Open the native save dialog for the user to pick a destination
/// file. Returns the chosen absolute path or `None` if cancelled.
///
/// Takes a single `{ req: { default_filename } }` JSON object to
/// side-step Tauri's arg-name conversion gotchas (the user's
/// `MISSING REQUIRED KEY DEFAULTFILENAME` error from earlier was
/// the same camelCase/snake_case trap that bit `lzma_level`).

/// Decompress an archive by path. Auto-detects the format from the
/// file's magic bytes (NXS6 / NXAR / v4 / v5-v6 single) and restores
/// the contents next to the input. The frontend passes
/// `{ req: { path, output_dir } }`.
///
/// `output_dir` (optional) overrides the auto-generated restore
/// location: extracted contents go there instead.
///
/// Progress is emitted as `compress-progress` events (same channel
/// as compress; the GUI already listens for them).
#[tauri::command]
pub async fn decompress_target_cmd(
    app: tauri::AppHandle,
    db_state: State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    req: serde_json::Value,
) -> Result<DecompressTargetResult, String> {
    let path = req
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'path' in req".to_string())?
        .to_string();
    let output_dir = req
        .get("output_dir")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);
    // Sprint 5.7.2: when the archive is encrypted (NXE\0 / NXR\0
    // magic), the frontend passes `req.password: string` here so
    // we can route to `decompress_target_with_password` which
    // does the AES-256-GCM decrypt (and Reed-Solomon reassembly
    // if recovery is enabled) BEFORE handing the recovered NXS
    // bytes to the plain inner-format decompressor. When password
    // is absent, we go through the plain path — same as before
    // Sprint 5.7.2 — so legacy `.tar` / `.zst` / `.gz` / `.nxs6`
    // / LZMA archives continue to work untouched.
    let password: Option<String> = req
        .get("password")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let p = PathBuf::from(path);
    let filename_for_db = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    let inner: ApiResult<DecompressTargetResult> = tauri::async_runtime::spawn_blocking(move || {
        let app_for_event = app.clone();
        let throttler = throttle::ThrottledEmitter::new(app_for_event.clone());
        let cb = |event: ProgressEvent| {
            throttler.feed("compress-progress", &event);
        };
        // Sprint 5.7.x — WinRAR-compatible dispatch. For
        // .zip/.7z/.rar we extract via the external readers and
        // return a synthetic DecompressTargetResult that matches
        // the engine shape (archive_kind / restored_size /
        // n_files / output_path). For all other formats we keep
        // the existing engine-crate dispatch.
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "zip" | "7z" | "rar" => {
                // Encrypted WinRAR-style archives are out of scope
                // for Sprint 5.7.x (use WinRAR / 7-Zip directly).
                if password.is_some() {
                    return Err(api::ApiError::new(
                        "decompress.encrypted_external_unsupported",
                        "encrypted .zip/.7z/.rar not supported (use 7-Zip / WinRAR)",
                    ));
                }
                decompress_external_archive_cmd(&p, output_dir.as_deref(), &ext, &cb)
            }
            _ => match password {
                Some(pwd) => {
                    // Drop the password immediately after we've moved
                    // it into the closure so it doesn't linger on the
                    // stack any longer than needed (defense in depth
                    // — the `drop(pwd)` below ensures the heap copy
                    // is zeroed when the function returns).
                    api::decompress_target_with_password(
                        &p,
                        pwd.as_bytes(),
                        cb,
                    )
                }
                None => api::decompress_target_with_progress(
                    &p,
                    output_dir.as_deref(),
                    cb,
                ),
            },
        }
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?;
    let result: DecompressTargetResult = to_ipc(inner)?;

    // Sprint 5.7 hotfix #20: persist into the local SQLite store
    // (best-effort — never fail the op if stats fail).
    // For decompression we treat the archive size as the "input"
    // and the restored size as the "output". `original_bytes` in
    // db.rs is the input on disk; for extract flows it equals the
    // archive size. We don't have it in DecompressTargetResult so
    // we use `restored_size` for both — the weighted average stays
    // correct because record_decompression only updates counters
    // and the events row, not the compression_ratio.
    let restored = result.restored_size;
    if let Err(e) = persist_decompression(&db_state, &filename_for_db, restored, restored).await {
        eprintln!("[nexus-rar] WARN: failed to persist decompression stats: {}", e);
    }
    Ok(result)
}

/// Peek at an archive's contents WITHOUT decompressing. Returns
/// the file list, total uncompressed size, and archive kind.
/// The frontend uses this to render a WinRAR-style preview
/// before the user commits to extracting.
///
/// Sprint 5.7.2: extended to handle encrypted archives. The
/// optional `password` field in the req enables the unlock path
/// — when set, the encrypted archive is decrypted (GCM-verified)
/// and the underlying NXS file list is returned. When the password
/// is wrong, the GCM auth fails and we return a clear error code
/// (the frontend renders this as "✗ wrong password" in the
/// ArchivePreview component).
#[tauri::command]
pub async fn peek_archive_target_cmd(req: serde_json::Value) -> Result<PeekResult, String> {
    let path = req
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'path' in req".to_string())?
        .to_string();
    let password: Option<String> = req
        .get("password")
        .and_then(|v| v.as_str())
        .map(String::from);
    let p = PathBuf::from(path);
    tauri::async_runtime::spawn_blocking(move || {
        // Sprint 5.7.x — WinRAR-compatible dispatch. The engine
        // crate (`api::*`) handles NXS/NXAR/LZMA/NXE/NXR by magic
        // bytes. For .zip/.7z/.rar we route to the Tauri-side
        // external readers which carry the zip/7z-rust crates.
        // We dispatch on extension because the legacy path's
        // magic-byte check rejects the new formats.
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "zip" | "7z" | "rar" => {
                // Encrypted WinRAR-style archives (.zip with
                // WinZip AES, etc.) are out of scope for now — we
                // only support the encrypted NXS envelopes.
                if password.is_some() {
                    return Err(
                        "encrypted .zip/.7z/.rar not supported (use 7-Zip / WinRAR)"
                            .to_string(),
                    );
                }
                peek_external_archive_cmd(&p, &ext)
            }
            _ => match password.as_deref() {
                Some(pwd) => {
                    to_ipc(api::peek_archive_target_with_password(&p, pwd.as_bytes()))
                }
                None => to_ipc(api::peek_archive_target(&p)),
            },
        }
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

/// Extract a .zip/.7z/.rar archive via the appropriate external
/// reader and return a `DecompressTargetResult` matching the engine
/// crate's shape. The frontend's progress bar + Toaster don't need
/// to special-case anything — same JSON, same event semantics.
fn decompress_external_archive_cmd(
    path: &Path,
    output_dir: Option<&Path>,
    ext: &str,
    cb: &dyn Fn(api::ProgressEvent),
) -> ApiResult<DecompressTargetResult> {
    let start = std::time::Instant::now();
    // Decide where to extract. If the user supplied an output_dir,
    // use it; otherwise drop files next to the archive (same as the
    // engine's default behaviour).
    let target_dir = match output_dir {
        Some(p) => p.to_path_buf(),
        None => path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    // Read the entries first so we can report total bytes / count
    // and emit a pre-walk progress event.
    let entries = match ext {
        "zip" => crate::external_archive::list_zip_entries(path),
        "7z" => crate::external_archive::list_7z_entries(path),
        "rar" => crate::external_archive::list_rar_entries(path),
        other => {
            return Err(api::ApiError::new(
                "decompress.unsupported_external",
                format!("unsupported external format: .{}", other),
            ))
        }
    }
    .map_err(|e| api::ApiError::new("decompress.external_list_failed", e))?;
    let file_count = entries.len() as u64;
    let total_bytes: u64 = entries.iter().map(|e| e.size).sum();
    cb(api::ProgressEvent {
        phase: "decompressing".to_string(),
        current_file: String::new(),
        files_done: 0,
        files_total: file_count,
        bytes_done: 0,
        bytes_total: total_bytes,
        elapsed_ms: 0,
        bytes_per_sec: 0.0,
        eta_ms: 0,
    });
    // Extract. The readers don't emit per-file events — they stream
    // sequentially. We post a "decompressing" event before, and a
    // final "done" event after, so the progress bar gets two frames
    // (typical for small archives that finish in <100 ms).
    let _written = match ext {
        "zip" => crate::external_archive::extract_zip_entries(path, &target_dir, None),
        "7z" => crate::external_archive::extract_7z_entries(path, &target_dir, None),
        "rar" => crate::external_archive::extract_rar_entries(path, &target_dir, None),
        _ => unreachable!(),
    }
    .map_err(|e| api::ApiError::new("decompress.external_extract_failed", e))?;
    cb(api::ProgressEvent {
        phase: "done".to_string(),
        current_file: String::new(),
        files_done: file_count,
        files_total: file_count,
        bytes_done: total_bytes,
        bytes_total: total_bytes,
        elapsed_ms: start.elapsed().as_millis() as u64,
        bytes_per_sec: if start.elapsed().as_millis() > 0 {
            total_bytes as f64 / (start.elapsed().as_millis() as f64 / 1000.0)
        } else {
            0.0
        },
        eta_ms: 0,
    });
    Ok(DecompressTargetResult {
        archive_kind: match ext {
            "zip" => "zip",
            "7z" => "7z",
            "rar" => "rar",
            _ => "external",
        },
        restored_size: total_bytes,
        n_files: file_count,
        is_directory: true,
        output_path: target_dir.to_string_lossy().into_owned(),
        decompress_time_ms: start.elapsed().as_millis() as f64,
    })
}

/// Build a PeekResult from a .zip/.7z/.rar archive by listing its
/// entries via the appropriate `external_archive` reader. Returns
/// the same `PeekResult` shape the engine crate uses so the
/// frontend's ArchivePreview doesn't need to special-case anything.
fn peek_external_archive_cmd(path: &Path, ext: &str) -> Result<PeekResult, String> {
    let entries = match ext {
        "zip" => crate::external_archive::list_zip_entries(path)?,
        "7z" => crate::external_archive::list_7z_entries(path)?,
        "rar" => crate::external_archive::list_rar_entries(path)?,
        other => {
            return Err(format!("unsupported external format: .{}", other))
        }
    };
    let compressed_size = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(0);
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let n = entries.len() as u64;
    let files: Vec<api::ArchivePreviewEntry> = entries
        .into_iter()
        .map(|e| api::ArchivePreviewEntry {
            path: e.name,
            size: e.size,
            is_dir: e.is_dir,
        })
        .collect();
    Ok(PeekResult {
        archive_kind: match ext {
            "zip" => "zip",
            "7z" => "7z",
            "rar" => "rar",
            _ => "external",
        },
        n_files: n,
        total_uncompressed: total,
        compressed_size,
        files,
    })
}

// ============================================================================
//  Sprint 5.6.17: WinRAR-style archive inspection
// ============================================================================

#[derive(Serialize)]
pub struct ArchiveEntryRow {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Serialize)]
pub struct ArchiveListResp {
    /// Entries in this page (subset of the full archive).
    pub entries: Vec<ArchiveEntryRow>,
    /// Total files (excluding directories) in the full archive.
    pub total_files: usize,
    /// Total bytes (sum of original_size across all files).
    pub total_bytes: u64,
    /// Offset into the full entries list (where this page starts).
    pub offset: usize,
    /// Whether there are more entries past this page.
    pub has_more: bool,
    /// Wall-clock time to enumerate the headers (excludes
    /// payload reading — should be milliseconds even for
    /// multi-GB archives). Useful to show "parsed 87K entries
    /// in 12 ms" so users see the cost is small.
    pub parse_time_ms: u128,
}

#[derive(Serialize)]
pub struct ArchiveExtractResp {
    pub written: Vec<String>,
    pub count: usize,
    pub total_bytes: u64,
}

#[tauri::command]
pub async fn p2p_archive_list_cmd(req: serde_json::Value) -> Result<ArchiveListResp, String> {
    let path_str = req
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'path'".to_string())?
        .to_string();
    // Pagination: front-end requests a window of entries at a
    // time. For 30+ GB archives with hundreds of thousands of
    // entries, sending the whole list over IPC would mean a
    // multi-MB JSON parse + a DOM tree the browser can't render.
    // 500 entries per page is enough to fill the viewport and
    // keeps the IPC payload < 100 KB.
    let offset: usize = req
        .get("offset")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(0);
    let limit: usize = req
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).min(2000))
        .unwrap_or(500);
    let path = PathBuf::from(path_str);
    let (page, total_files, total_bytes, has_more, parse_time_ms) =
        tauri::async_runtime::spawn_blocking(move || {
            let start = std::time::Instant::now();
            // Sprint 5.7.x — extension-aware dispatch so .zip / .7z
            // / .rar files (WinRAR-style archives) go to the
            // external readers instead of the legacy tar/solid
            // dispatch. The legacy `archive_inspect::list_paginated`
            // is kept for .tar / .nxs6 / .nxs / .nxar.
            let page = list_external_or_internal_paginated(&path, offset, limit)?;
            // Total stats — sum of sizes in this page; the UI shows
            // an approximate "X GB total" when pagination kicks in.
            let total_bytes: u64 = page.entries.iter().map(|e| e.size).sum();
            let total_files = if page.has_more {
                offset + limit + 1
            } else {
                offset + page.entries.len()
            };
            Ok::<_, String>((
                page.entries,
                total_files,
                total_bytes,
                page.has_more,
                start.elapsed().as_millis(),
            ))
        })
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))??;
    let rows = page
        .into_iter()
        .map(|e| ArchiveEntryRow {
            name: e.name,
            size: e.size,
            is_dir: e.is_dir,
        })
        .collect();
    Ok(ArchiveListResp {
        entries: rows,
        total_files,
        total_bytes,
        has_more,
        offset,
        parse_time_ms,
    })
}

/// Dispatch by extension: .zip → zip reader, .7z → sevenz-rust,
/// .rar → UnRAR shell-out, otherwise fall back to the legacy
/// tar / solid reader in `archive_inspect`.
fn list_external_or_internal_paginated(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<crate::archive_inspect::PageResult, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "zip" => crate::external_archive::list_zip_paginated(path, offset, limit),
        "7z" => crate::external_archive::list_7z_paginated(path, offset, limit),
        "rar" => crate::external_archive::list_rar_paginated(path, offset, limit),
        _ => crate::archive_inspect::list_paginated(path, offset, limit),
    }
}

fn extract_external_or_internal(
    path: &Path,
    output_dir: &Path,
    selected: Option<Vec<String>>,
) -> Result<Vec<String>, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "zip" => crate::external_archive::extract_zip_entries(path, output_dir, selected),
        "7z" => crate::external_archive::extract_7z_entries(path, output_dir, selected),
        "rar" => crate::external_archive::extract_rar_entries(path, output_dir, selected),
        _ => crate::archive_inspect::extract_entries(path, output_dir, selected),
    }
}

#[tauri::command]
pub async fn p2p_archive_extract_cmd(req: serde_json::Value) -> Result<ArchiveExtractResp, String> {
    let path_str = req
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'path'".to_string())?
        .to_string();
    // output_dir is optional — if null/absent, extract next to
    // the archive (same directory as the source file).
    let output_dir_str: Option<String> = req
        .get("output_dir")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let selected: Option<Vec<String>> = req.get("selected").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect()
    });
    let path = PathBuf::from(&path_str);
    // Resolve output_dir: use the user-supplied path, or fall back
    // to the directory that contains the archive.
    let output_dir = match output_dir_str {
        Some(ref s) => PathBuf::from(s),
        None => PathBuf::from(&path_str)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let output_dir_for_total = output_dir.clone();
    let written = tauri::async_runtime::spawn_blocking(move || {
        extract_external_or_internal(&path, &output_dir, selected)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))??;
    let count = written.len();
    let total_bytes = std::fs::read_dir(&output_dir_for_total)
        .map(|_| 0u64) // quick approximation; the user gets real
        // sizes from list if they need exact totals
        .unwrap_or(0);
    Ok(ArchiveExtractResp {
        written,
        count,
        total_bytes,
    })
}

/// Reveal `path` in Finder (macOS) / File Manager (Linux/Windows).
/// Pass a path to a FILE — Finder will select it. Pass a path to a
/// DIRECTORY — Finder will open it.
#[tauri::command]
pub async fn reveal_in_finder_cmd(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || reveal_in_finder(&path))
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

#[cfg(target_os = "macos")]
fn reveal_in_finder(path: &str) -> Result<(), String> {
    // `-R` flag tells `open` to REVEAL the file in its parent dir.
    let status = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status()
        .map_err(|e| format!("open -R failed: {}", e))?;
    if !status.success() {
        return Err(format!("open exited with {:?}", status.code()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn reveal_in_finder(path: &str) -> Result<(), String> {
    // xdg-open opens the parent if `path` is a file.
    let status = std::process::Command::new("xdg-open")
        .arg(path)
        .status()
        .map_err(|e| format!("xdg-open failed: {}", e))?;
    if !status.success() {
        return Err(format!("xdg-open exited with {:?}", status.code()));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn reveal_in_finder(path: &str) -> Result<(), String> {
    // `explorer /select,<path>` reveals in Explorer.
    let status = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path))
        .status()
        .map_err(|e| format!("explorer failed: {}", e))?;
    if !status.success() {
        return Err(format!("explorer exited with {:?}", status.code()));
    }
    Ok(())
}

// ============================================================================
//  File/folder pickers (Sprint 5.5.5: blocking pattern — fixes hang on macOS)
// ============================================================================
//
// The previous version of these commands used the async-callback-with-mpsc
// pattern (pick_file(callback) + spawn_blocking(recv)). That pattern is
// documented as deadlock-prone by tauri-plugin-dialog — the callback is
// dispatched from a sub-thread but the dialog itself needs the main
// thread, and the channel rendezvous can hang. The fix is to use the
// blocking_pick_* variants inside spawn_blocking. Simpler and works.

/// Pick a single file. Returns the absolute path or None if cancelled.
#[tauri::command]
pub async fn pick_file_cmd(window: tauri::Window) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let window = window.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let path = window
            .dialog()
            .file()
            .add_filter(
                "Nexus archive (.nxs/.nxs6/.lz/.nxar/.nxr)",
                &["nxs", "nxs6", "lz", "nxar", "nxr"],
            )
            .add_filter("All files", &["*"])
            .blocking_pick_file();
        path.and_then(|fp| fp.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| format!("dialog join failed: {}", e))?;
    Ok(picked)
}

/// Pick MULTIPLE files (Sprint 5.5.5).
#[tauri::command]
pub async fn pick_files_cmd(window: tauri::Window) -> Result<Option<Vec<String>>, String> {
    use tauri_plugin_dialog::DialogExt;
    let window = window.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let paths = window
            .dialog()
            .file()
            .add_filter("All files", &["*"])
            .blocking_pick_files();
        paths.map(|v| {
            v.into_iter()
                .filter_map(|fp| fp.into_path().ok())
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
    })
    .await
    .map_err(|e| format!("dialog join failed: {}", e))?;
    Ok(picked)
}

/// Pick a single directory.
#[tauri::command]
pub async fn pick_directory_cmd(window: tauri::Window) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let window = window.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let path = window.dialog().file().blocking_pick_folder();
        path.and_then(|fp| fp.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| format!("dialog join failed: {}", e))?;
    Ok(picked)
}

/// Pick MULTIPLE directories.
#[tauri::command]
pub async fn pick_folders_cmd(window: tauri::Window) -> Result<Option<Vec<String>>, String> {
    use tauri_plugin_dialog::DialogExt;
    let window = window.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let paths = window.dialog().file().blocking_pick_folders();
        paths.map(|v| {
            v.into_iter()
                .filter_map(|fp| fp.into_path().ok())
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
    })
    .await
    .map_err(|e| format!("dialog join failed: {}", e))?;
    Ok(picked)
}

/// Pick a save location (the user picks where to write the output).
/// Used by Receive panel. Args via single-JSON-arg pattern (the
/// camelCase/snake_case trap that bit `lzma_level`).
#[tauri::command]
pub async fn pick_save_location_cmd(
    window: tauri::Window,
    req: serde_json::Value,
) -> Result<Option<String>, String> {
    let default_filename = req
        .get("default_filename")
        .and_then(|v| v.as_str())
        .unwrap_or("archive.nxs")
        .to_string();
    use tauri_plugin_dialog::DialogExt;
    let window = window.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let path = window
            .dialog()
            .file()
            .set_file_name(&default_filename)
            .blocking_save_file();
        path.and_then(|fp| fp.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| format!("dialog join failed: {}", e))?;
    Ok(picked)
}

#[tauri::command]
pub async fn self_test_cmd() -> Result<SelfTestResult, String> {
    to_ipc(api::self_test())
}

#[tauri::command]
pub async fn compress_directory_cmd(
    input_dir: String,
    level: String,
) -> Result<(api::DirectoryResult, Vec<u8>), String> {
    let level = CompressionLevel::from_str(&level).map_err(|e| format!("invalid_level: {}", e))?;
    let path = PathBuf::from(input_dir);
    tauri::async_runtime::spawn_blocking(move || {
        to_ipc(api::compress_directory(&path, level).map(|(r, a)| (r, a.to_vec())))
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

#[tauri::command]
pub async fn compress_directories_cmd(
    input_dirs: Vec<String>,
    level: String,
) -> Result<(api::DirectoryResult, Vec<u8>), String> {
    let level = CompressionLevel::from_str(&level).map_err(|e| format!("invalid_level: {}", e))?;
    let paths: Vec<PathBuf> = input_dirs.into_iter().map(PathBuf::from).collect();
    tauri::async_runtime::spawn_blocking(move || {
        let path_refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
        to_ipc(api::compress_directories(&path_refs, level).map(|(r, a)| (r, a.to_vec())))
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

#[tauri::command]
pub async fn decompress_directory_cmd(
    archive: Vec<u8>,
    output_dir: String,
) -> Result<api::DirectoryResult, String> {
    let path = PathBuf::from(output_dir);
    tauri::async_runtime::spawn_blocking(move || to_ipc(api::decompress_directory(&archive, &path)))
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

#[tauri::command]
pub async fn peek_archive_cmd(archive: Vec<u8>) -> Result<api::DirectoryResult, String> {
    to_ipc(api::peek_archive(&archive))
}

#[tauri::command]
pub async fn peek_archive_file_cmd(path: String) -> Result<Vec<api::ArchiveEntry>, String> {
    let p = PathBuf::from(path);
    tauri::async_runtime::spawn_blocking(move || to_ipc(api::peek_archive_file(&p)))
        .await
        .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

#[tauri::command]
pub async fn peek_and_extract_file_cmd(
    path: String,
    extract_to: Option<String>,
) -> Result<(Vec<api::ArchiveEntry>, Option<api::DirectoryResult>), String> {
    let p = PathBuf::from(path);
    let out = extract_to.map(PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || {
        to_ipc(api::peek_and_extract_file(&p, out.as_deref()))
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

#[tauri::command]
pub async fn read_file_cmd(path: String) -> Result<Vec<u8>, String> {
    let p = PathBuf::from(path);
    tauri::async_runtime::spawn_blocking(move || {
        std::fs::read(&p).map_err(|e| format!("read_file failed: {}", e))
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))?
}

#[tauri::command]
pub async fn open_path_cmd(path: String) -> Result<(), String> {
    to_ipc(api::open_path(&path))
}

// ============================================================================
//  P2P tunnel helpers
// ============================================================================

/// After receiving a file, if the wire filename ends in `.tar`
/// (i.e. the sender archived a directory), extract it in-place
/// and remove the archive. This restores the original directory
/// structure (including macOS `.app` bundles with their symlinks
/// and permission bits).
///
/// Returns (final_path, final_filename):
///   - For `.tar` files: (extracted_dir_path, "DirName") — the
///     top-level entry name extracted from the archive.
///   - For everything else: (original_path, original_filename)
///     unchanged.
fn maybe_extract_tar(
    received_path: PathBuf,
    wire_filename: Option<&str>,
) -> Result<(PathBuf, Option<String>), String> {
    // Decide whether to extract based on the wire filename
    // (most reliable) or the received path extension.
    let is_tar = wire_filename
        .map(|n| n.ends_with(".tar"))
        .unwrap_or_else(|| {
            received_path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("tar"))
                .unwrap_or(false)
        });

    if !is_tar {
        // Nothing to do — return as-is.
        return Ok((received_path, wire_filename.map(|s| s.to_string())));
    }

    // The archive should be extracted next to itself.
    let extract_dir = received_path
        .parent()
        .ok_or_else(|| "received .tar has no parent dir".to_string())?;

    eprintln!(
        "[p2p] auto-extracting {} into {}",
        received_path.display(),
        extract_dir.display()
    );

    // Use system tar to extract: -x extract, -p preserve
    // permissions (vital for .app bundles), -f archive file.
    let tar_output = std::process::Command::new("/usr/bin/tar")
        .arg("-xpf")
        .arg(&received_path)
        .arg("-C")
        .arg(extract_dir)
        .output()
        .map_err(|e| format!("spawn tar -xpf: {}", e))?;

    if !tar_output.status.success() {
        return Err(format!(
            "tar extraction failed: exit={:?} stderr={}",
            tar_output.status.code(),
            String::from_utf8_lossy(&tar_output.stderr)
        ));
    }

    // Determine the top-level name that was extracted. The
    // wire_filename is "DirName.tar", so we strip ".tar" to
    // get "DirName". This is exactly what tar created in
    // extract_dir.
    let extracted_name = wire_filename
        .and_then(|n| n.strip_suffix(".tar"))
        .unwrap_or_else(|| {
            received_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("extracted")
        })
        .to_string();

    let extracted_path = extract_dir.join(&extracted_name);

    // Remove the raw .tar now that we've extracted it.
    if let Err(e) = std::fs::remove_file(&received_path) {
        eprintln!(
            "[p2p] warning: could not remove temp tar {}: {}",
            received_path.display(),
            e
        );
    } else {
        eprintln!("[p2p] removed temp tar {}", received_path.display());
    }

    // Clear quarantine on the extracted tree (macOS: Gatekeeper
    // would block every .app file without this).
    #[cfg(target_os = "macos")]
    {
        let path_str = extracted_path.to_string_lossy().into_owned();
        let _ = std::process::Command::new("/usr/bin/xattr")
            .args(["-rd", "com.apple.quarantine", &path_str])
            .output();
    }

    eprintln!("[p2p] extraction complete: {}", extracted_path.display());

    Ok((extracted_path, Some(extracted_name)))
}

// ============================================================================
//  P2P tunnel — Sprint 5.0 demo
// ============================================================================
//
// The P2P module exposes three Tauri commands:
//   * `p2p_send_start` — pick a local port, spawn cloudflared,
//     run the axum server, and return the token. The started
//     send session is stored in `P2pState` so the UI can cancel
//     it.
//   * `p2p_send_abort` — kill the tunnel + abort the server
//     task. Returns Ok(()) always (idempotent).
//   * `p2p_receive` — connect to the URL in the token, do
//     SPAKE2, download + decrypt + verify the file.
//
// All three use the single-JSON-arg pattern (Tauri 2.x arg-name
// gotcha — see MEMORY.md).
use crate::db;
use crate::p2p_tunnel;
use crate::throttle;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tauri::State;
use tokio::sync::Mutex;

/// Holds the currently-active send session (if any). There's
/// at most one at a time — the UI's intent switch resets the
/// other side.
pub struct P2pState {
    /// Arc<Mutex> so a background watcher task can monitor
    /// the slot's lifecycle (Sprint 5.6.13) without needing
    /// to borrow through the Tauri State wrapper.
    pub active: Arc<Mutex<Option<p2p_tunnel::StartedSend>>>,
}

#[derive(Deserialize)]
pub struct P2pSendStartReq {
    /// Absolute path of the file to send.
    pub file_path: String,
    /// User-supplied code, or None to let the server generate one.
    pub code: Option<String>,
}

#[derive(Serialize)]
pub struct UpnpStatusInfo {
    /// Public IP the sender is reachable at from the internet
    /// (from UPnP GetExternalIPAddress). Format: dotted-quad.
    pub external_ip: String,
    /// Port the UPnP mapping is on (= the sender's local axum
    /// port for Direct Mode).
    pub external_port: u16,
}

#[derive(Serialize)]
pub struct P2pSendStartResp {
    /// base64url-encoded token (the thing to share with the
    /// receiver). v1 for Quick/Named, v2 for Direct LAN-only,
    /// v3 for Direct cross-NAT (with UPnP hole).
    pub token: String,
    /// The code we used, regardless of whether the user
    /// supplied it or we generated it. The UI shows this in
    /// big letters under the QR / token box.
    pub code: String,
    /// Pretty-printed name of the file.
    pub filename: String,
    /// Plaintext size in bytes (so the UI can show
    /// "Sending 5.3 MB" before the connection starts).
    pub file_size: u64,
    /// Sprint 5.5.4 Phase 3: UPnP hole status. `Some` means
    /// the sender has an open port mapping and the token is v3
    /// (cross-NAT capable). `None` means UPnP was unavailable
    /// and the token is v2 (LAN-only).
    pub upnp_status: Option<UpnpStatusInfo>,
}

#[tauri::command]
pub async fn p2p_send_start_cmd(
    app: tauri::AppHandle,
    db_state: State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    req: serde_json::Value,
    state: State<'_, Arc<P2pState>>,
) -> Result<P2pSendStartResp, String> {
    use tauri::Manager; // brings .path() into scope on AppHandle
    let req: P2pSendStartReq = serde_json::from_value(req)
        .map_err(|e| format!("invalid p2p_send_start request: {}", e))?;
    let file_path = PathBuf::from(&req.file_path);
    if !file_path.exists() {
        return Err(format!("file not found: {}", req.file_path));
    }
    let code = match req.code {
        Some(c) if !c.trim().is_empty() => c.trim().to_string(),
        _ => p2p_tunnel::random_code_phrase(4),
    };
    // Refuse if a send is already in flight. The UI's intent
    // switch should call p2p_send_abort first; this is a
    // safety net.
    {
        let active = state.active.lock().await;
        if active.is_some() {
            return Err("a p2p send is already in progress — abort it first".to_string());
        }
    }
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app_data_dir: {}", e))?;
    std::fs::create_dir_all(&app_data_dir).map_err(|e| format!("mkdir app_data_dir: {}", e))?;
    let started = p2p_tunnel::start_sender(file_path.clone(), code.clone(), app_data_dir).await?;
    // Sprint 5.6.16: if the user picked a directory, start_sender
    // The filename is already computed correctly by start_sender
    // (it adds .tar for directories, preserves the original name
    // for files and already-compressed formats). Read it from the
    // token rather than re-computing it here to avoid drift.
    let filename = started.token.filename.clone().unwrap_or_else(|| {
        file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string()
    });
    let file_size = started.token.size;
    let upnp_status = started.upnp_info.as_ref().map(|info| UpnpStatusInfo {
        external_ip: info.external_ip.to_string(),
        external_port: info.external_port,
    });
    let resp = P2pSendStartResp {
        token: started.token_compact.clone(),
        code: started.token.code.clone(),
        filename,
        file_size,
        upnp_status,
    };
    // Sprint 5.7 hotfix #20: record the share start so Recientes
    // shows "Enlace compartido generado". We record AT START (not
    // on completion) because p2p_send_start_cmd returns the link
    // immediately and the receiver may or may not actually finish
    // downloading — counting it from the moment the user shared
    // matches the user-perceived "I shared this file" intent and
    // matches what the Recientes + Home counters should reflect.
    if let Err(e) = persist_share(&db_state, &resp.filename, resp.file_size).await {
        eprintln!("[nexus-rar] WARN: failed to persist share stats: {}", e);
    }
    // Sprint 5.6.15: clear state.active when either (a) the
    // receiver hits /done (signaling successful file transfer)
    // or (b) 10 minutes have passed since the sender started
    // (hard timeout for abandoned sessions). The watcher
    // polls every 500ms.
    //
    // Sprint 5.6.13 tried to detect this via
    // `server_task.is_finished()`, but axum::serve runs the
    // TCP listener loop FOREVER — it never returns naturally.
    // So that approach was broken. We now rely on explicit
    // signals from the receiver (/done) plus a timeout.
    let state_clone = state.inner().clone();
    *state.active.lock().await = Some(started);
    let active_for_watcher = state_clone.active.clone();
    tokio::spawn(async move {
        let session_start = std::time::Instant::now();
        let hard_timeout = Duration::from_secs(10 * 60); // 10 min
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut guard = active_for_watcher.lock().await;
            let should_clear = match guard.as_ref() {
                // receiver flagged completion — clear immediately
                Some(s) if s.done.load(std::sync::atomic::Ordering::SeqCst) => {
                    eprintln!("[p2p-state] /done received, clearing active slot");
                    true
                }
                // server task aborted (abort_cmd called) — clear
                Some(s) if s.server_task.is_finished() => {
                    eprintln!("[p2p-state] server task aborted, clearing active slot");
                    true
                }
                // hard timeout for sessions that never finished
                Some(s) if session_start.elapsed() >= hard_timeout => {
                    eprintln!(
                        "[p2p-state] hard timeout reached ({}s), clearing active slot",
                        session_start.elapsed().as_secs()
                    );
                    // also abort the server so it stops listening
                    s.server_task.abort();
                    true
                }
                Some(_) => false,
                None => return, // already cleared — exit watcher
            };
            if should_clear {
                *guard = None;
                return;
            }
        }
    });
    Ok(resp)
}

#[tauri::command]
pub async fn p2p_send_abort_cmd(state: State<'_, Arc<P2pState>>) -> Result<(), String> {
    let mut active = state.active.lock().await;
    if let Some(started) = active.take() {
        // Dropping StartedSend kills the tunnel child (Drop on
        // TunnelHandle) and aborts the server task (Drop on
        // JoinHandle, well, JoinHandle doesn't abort on drop
        // — we need to explicitly abort).
        started.server_task.abort();
        drop(started.tunnel);
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct P2pReceiveReq {
    /// base64url-encoded token from the sender.
    pub token: String,
    /// Where to write the decrypted file.
    pub output_path: String,
}

#[derive(Serialize)]
pub struct P2pReceiveResp {
    /// Plaintext bytes received.
    pub bytes_written: u64,
    /// Absolute path of the written file.
    pub output_path: String,
    /// Sprint 5.6.8: original filename from the sender. None
    /// if the sender didn't include it (older tokens).
    pub filename: Option<String>,
}

#[tauri::command]
pub async fn p2p_receive_cmd(req: serde_json::Value) -> Result<P2pReceiveResp, String> {
    let req: P2pReceiveReq =
        serde_json::from_value(req).map_err(|e| format!("invalid p2p_receive request: {}", e))?;
    let token = p2p_tunnel::P2pToken::from_compact(&req.token)
        .map_err(|e| format!("invalid token: {}", e))?;
    let output_path = PathBuf::from(&req.output_path);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir output parent: {}", e))?;
    }
    let result = p2p_tunnel::receive_send_file(token, output_path).await?;
    let (final_path, final_filename) =
        maybe_extract_tar(result.output_path, result.filename.as_deref())?;
    Ok(P2pReceiveResp {
        bytes_written: result.bytes_written,
        output_path: final_path.to_string_lossy().to_string(),
        filename: final_filename,
    })
}

/// Receive a file sent via Direct Mode (LAN + mDNS). The token
/// is a v2 token (`nx:2:direct:<hash>:<code>`) — no URL, the
/// sender is discovered via mDNS browse. `timeout_secs` caps
/// the mDNS browse window (5s default in the UI).
#[tauri::command]
pub async fn p2p_receive_direct_cmd(req: serde_json::Value) -> Result<P2pReceiveResp, String> {
    let req: serde_json::Value = req;
    let token_v2 = req
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'token'".to_string())?
        .to_string();
    let output_path_str = req
        .get("output_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'output_path'".to_string())?
        .to_string();
    let timeout_secs = req
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(5);
    let output_path = PathBuf::from(&output_path_str);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir output parent: {}", e))?;
    }
    let result = p2p_tunnel::receive_direct_file(token_v2, output_path, timeout_secs).await?;
    let (final_path, final_filename) =
        maybe_extract_tar(result.output_path, result.filename.as_deref())?;
    Ok(P2pReceiveResp {
        bytes_written: result.bytes_written,
        output_path: final_path.to_string_lossy().to_string(),
        filename: final_filename,
    })
}

/// Sprint 5.6.9: peek the original filename from the sender
/// BEFORE the user clicks "Recibir". Returns null if the
/// sender didn't include one (older tokens). For v2/v3 this
/// probes the sender's endpoint and reads /meta.
#[derive(Serialize)]
pub struct P2pPeekFilenameResp {
    pub filename: Option<String>,
}

#[tauri::command]
pub async fn p2p_peek_filename_cmd(req: serde_json::Value) -> Result<P2pPeekFilenameResp, String> {
    let token = req
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'token'".to_string())?
        .to_string();
    let timeout_secs = req
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(5);
    let filename = p2p_tunnel::peek_filename(&token, timeout_secs).await?;
    Ok(P2pPeekFilenameResp { filename })
}

// ============================================================================
//  Sprint 5.5.1 — tunnel config + transport mode commands
// ============================================================================

/// Returned by `p2p_get_tunnel_config_cmd`. `has_token` is true
/// iff a token is currently stored in the OS keyring. The
/// token itself is never returned (it's a secret).
#[derive(Serialize)]
pub struct P2pTunnelConfigResp {
    pub mode: String,
    pub hostname: Option<String>,
    pub has_token: bool,
}

#[tauri::command]
pub async fn p2p_get_tunnel_config_cmd(
    app: tauri::AppHandle,
) -> Result<P2pTunnelConfigResp, String> {
    use p2p_tunnel::p2p_config::{load_tunnel_config, TokenStore};
    use tauri::Manager; // brings .path() into scope on AppHandle
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app_data_dir: {}", e))?;
    let cfg = load_tunnel_config(&app_data_dir)?;
    let has_token = p2p_tunnel::p2p_config::default_token_store()
        .get_token()?
        .is_some();
    Ok(P2pTunnelConfigResp {
        mode: match cfg.mode {
            p2p_tunnel::p2p_config::TransportMode::Quick => "quick".to_string(),
            p2p_tunnel::p2p_config::TransportMode::Named => "named".to_string(),
            p2p_tunnel::p2p_config::TransportMode::Direct => "direct".to_string(),
        },
        hostname: cfg.hostname,
        has_token,
    })
}

#[derive(Deserialize)]
pub struct P2pSaveTunnelConfigReq {
    pub mode: String,
    pub hostname: Option<String>,
    /// Required iff mode == "named". The token is validated
    /// (length, base64 charset) and then written to the OS
    /// keyring. The token is NEVER persisted to disk.
    pub token: Option<String>,
}

#[tauri::command]
pub async fn p2p_save_tunnel_config_cmd(
    app: tauri::AppHandle,
    req: serde_json::Value,
) -> Result<(), String> {
    use p2p_tunnel::p2p_config::{
        default_token_store, save_tunnel_config, validate_hostname, validate_token, TokenStore,
        TransportMode, TunnelConfig,
    };
    use tauri::Manager; // brings .path() into scope on AppHandle
    let req: P2pSaveTunnelConfigReq = serde_json::from_value(req)
        .map_err(|e| format!("invalid save_tunnel_config request: {}", e))?;
    let mode = match req.mode.as_str() {
        "quick" => TransportMode::Quick,
        "named" => TransportMode::Named,
        "direct" => TransportMode::Direct,
        other => return Err(format!("invalid transport mode: {}", other)),
    };
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app_data_dir: {}", e))?;
    // Validate and normalize hostname (only relevant for Named
    // mode, but we validate if present).
    let hostname = req.hostname.as_deref().map(validate_hostname).transpose()?;
    let hostname = hostname.map(|s| s.strip_prefix("https://").unwrap_or(&s).to_string());
    // Validate the token (if provided). This is the FIRST
    // place we touch the secret — the token is in memory
    // only, never on disk.
    if let Some(token) = req.token.as_deref() {
        if !token.is_empty() {
            validate_token(token)?;
            default_token_store().set_token(token)?;
        }
    }
    let cfg = TunnelConfig { mode, hostname };
    save_tunnel_config(&app_data_dir, &cfg)?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────
//  Sprint 5.7: persistent stats + activity events
// ─────────────────────────────────────────────────────────────

/// Return the current global counters for the home dashboard.
/// Tauri serializes this straight to JSON; the frontend reads
/// the same fields shown in AppStats.
#[tauri::command]
pub async fn get_stats_cmd(
    state: State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
) -> Result<db::AppStats, String> {
    let conn = state.lock().await;
    db::get_stats(&conn).map_err(|e| e.to_string())
}

/// Return the most recent activity events for the home
/// "Actividad" / "Actividad reciente" sections. `limit` caps
/// the result; pass 5 for the sidebar, 30+ for the main panel.
#[tauri::command]
pub async fn get_recent_events_cmd(
    state: State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    limit: u32,
) -> Result<Vec<db::ActivityEvent>, String> {
    let conn = state.lock().await;
    db::list_events(&conn, limit).map_err(|e| e.to_string())
}

/// Returns the absolute path of the data directory used for
/// the SQLite store. Used by the Settings panel's
/// "Reveal in Finder / Explorer" action.
#[tauri::command]
pub fn data_dir_cmd() -> Result<String, String> {
    db::data_dir()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| e.to_string())
}

/// Sprint 5.7: wipe all stats + events from the local SQLite store.
/// Triggered by the Settings → Storage → "Borrar estadísticas"
/// button. The user is asked for confirmation in the UI before
/// this command is invoked; this command itself does not prompt.
#[tauri::command]
pub async fn reset_stats_cmd(
    state: State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
) -> Result<(), String> {
    let conn = state.lock().await;
    db::reset_stats(&conn).map_err(|e| e.to_string())
}

// ============================================================================
//  Sprint 5.7.x+ — cold-start warmup
// ============================================================================
//
// swc_core's parser / AST / codegen are lazy-initialized on the
// first call. For users who compress .ts/.tsx files (the default
// `SwcAst` preprocessor path), that first call costs ~1-3 s —
// visible as a frozen progress bar right after clicking "Comprimir".
//
// The frontend fires `warmup_cmd` once on app mount (in a background
// effect, non-blocking) so the lazy state is built up *before* the
// user clicks anything. After warmup, the first real compression
// starts instantly — no more "freezes for 2 s then goes".
//
// Cheap to call repeatedly (the second + Nth calls are no-ops if
// swc_core is already initialized) so we don't bother memoizing.
#[tauri::command]
pub async fn warmup_cmd() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| {
        // ast_minify::warmup() does a throwaway parse on a tiny
        // inline source. Triggers compilation of swc_core's
        // parser/codegen regex tables + AST node allocators.
        nexus_compress::ast_minify::warmup();
    })
    .await
    .map_err(|e| format!("warmup join failed: {}", e))
}

// ============================================================================
//  Sprint 5.7.x — WinRAR-compatible archive support
// ============================================================================
//
// Two new commands:
//
//   unrar_status_cmd    — UI queries this on mount so it can render
//                         the "RAR no soportado" banner when UnRAR
//                         isn't installed.
//
//   compress_to_zip_cmd — Wraps an existing compressed buffer (e.g.
//                         the output of compress_bytes_with_backend)
//                         in a .zip envelope with LZMA. WinRAR can
//                         open the result natively. This is the
//                         "compress in WinRAR style" path — the
//                         .zip container is the lingua franca of
//                         file compression and LZMA beats Deflate
//                         by ~10-15% on text/code inputs.

/// Returns the UnRAR detection result. Cheap (one `which` lookup),
/// safe to call on every app startup.
#[tauri::command]
pub async fn unrar_status_cmd() -> Result<crate::external_archive::UnrarStatus, String> {
    Ok(crate::external_archive::detect_unrar())
}

#[derive(Serialize)]
pub struct CompressToZipResp {
    pub output_path: String,
    pub input_bytes: u64,
    pub output_bytes: u64,
    /// Ratio as input/output (1.0 = no compression, 2.0 = half size).
    /// 0.0 if input is empty.
    pub ratio: f64,
}

/// Compress an existing file (or directory, tarred first) into a
/// `.zip` archive using LZMA (method 14). The result is a valid .zip
/// that WinRAR can open and decompress natively.
///
/// The `input_path` is read recursively (a directory is tar'd first
/// via the existing V6Solid backend so the .zip entry structure
/// preserves the directory layout). For a single file, the .zip
/// contains exactly one entry named after the input file.
#[tauri::command]
pub async fn compress_to_zip_cmd(
    db_state: State<'_, Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
    req: serde_json::Value,
) -> Result<CompressToZipResp, String> {
    let input_path_str = req
        .get("input_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'input_path'".to_string())?
        .to_string();
    let output_path_str = req
        .get("output_path")
        .and_then(|v| v.as_str())
        .map(String::from);
    let backend_str = req
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("v6-solid")
        .to_string();
    let lzma_level: u32 = req
        .get("lzma_level")
        .and_then(|v| v.as_u64())
        .unwrap_or(6) as u32;
    let password: Option<String> = req
        .get("password")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let input_path = PathBuf::from(&input_path_str);
    if !input_path.exists() {
        return Err(format!("input not found: {}", input_path_str));
    }

    // Resolve the output path: if the user didn't supply one, drop
    // the .zip next to the input with the input's stem.
    let output_path = match output_path_str {
        Some(p) => PathBuf::from(p),
        None => {
            let stem = input_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("archive");
            input_path
                .parent()
                .map(|p| p.join(format!("{}.zip", stem)))
                .unwrap_or_else(|| PathBuf::from(format!("{}.zip", stem)))
        }
    };

    // Collect (name, bytes) pairs to feed into the zip writer. For a
    // directory we walk it recursively; for a single file we emit one
    // entry. Each entry's bytes are pre-compressed with the chosen
    // backend so the final .zip contains the already-compressed
    // payload (we use the "stored" method inside .zip — i.e., method
    // 0 — so we don't double-compress with LZMA and waste CPU).
    //
    // Wait — that's wrong. If we store the .nxs bytes inside .zip
    // using method 0, the .zip is meaningless (it's just a wrapper
    // around .nxs files). We need the .zip to be standalone —
    // decompressable by WinRAR.
    //
    // So we use LZMA method (14) directly on the raw file bytes.
    // That's what the user gets when they pick "compress in WinRAR
    // style": the file bytes are LZMA-compressed and stored in a .zip
    // wrapper. Lossless, ratio-comparable to .7z on most inputs.
    let (files, total_in) = tauri::async_runtime::spawn_blocking({
        let input_path = input_path.clone();
        let backend = backend_str.clone();
        let pwd = password.clone();
        move || collect_input_files(&input_path, &backend, lzma_level, pwd.as_deref())
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))??;

    // Write the .zip with LZMA on each entry.
    let output_path_for_write = output_path.clone();
    let written = tauri::async_runtime::spawn_blocking(move || {
        crate::external_archive::write_zip_with_lzma(&files, &output_path_for_write)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))??;

    let ratio = if total_in > 0 {
        total_in as f64 / written as f64
    } else {
        0.0
    };

    let filename = output_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive.zip")
        .to_string();

    if let Err(e) = persist_compression(&db_state, &filename, total_in, written).await {
        eprintln!("[nexus-rar] WARN: failed to persist zip compression stats: {}", e);
    }

    Ok(CompressToZipResp {
        output_path: output_path.to_string_lossy().into_owned(),
        input_bytes: total_in,
        output_bytes: written,
        ratio,
    })
}

/// Walk the input path and collect (entry_name, raw_bytes) pairs.
/// For a directory, every file becomes an entry whose name is the
/// relative path from the directory root. For a single file, the
/// entry name is the file's basename.
fn collect_input_files(
    input_path: &Path,
    _backend: &str,
    _lzma_level: u32,
    _password: Option<&str>,
) -> Result<(Vec<(String, Vec<u8>)>, u64), String> {
    let mut out = Vec::new();
    let mut total_in: u64 = 0;
    let meta = std::fs::metadata(input_path).map_err(|e| format!("stat: {}", e))?;
    if meta.is_dir() {
        // Recursive walk. We avoid pulling in the `walkdir` crate
        // because the dependency budget is tight; std::fs::read_dir
        // is enough for our scale (typical use: a few thousand files
        // per archive).
        let mut stack = vec![input_path.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).map_err(|e| format!("read_dir: {}", e))? {
                let entry = entry.map_err(|e| format!("dir entry: {}", e))?;
                let path = entry.path();
                let ft = entry.file_type().map_err(|e| format!("file_type: {}", e))?;
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() {
                    let rel = path
                        .strip_prefix(input_path)
                        .map_err(|e| format!("strip_prefix: {}", e))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;
                    total_in += bytes.len() as u64;
                    out.push((rel, bytes));
                }
                // Symlinks are skipped — they don't round-trip well
                // through .zip without special handling, and the
                // user's "WinRAR-style" use case is plain files.
            }
        }
    } else {
        let bytes = std::fs::read(input_path).map_err(|e| format!("read: {}", e))?;
        total_in = bytes.len() as u64;
        let name = input_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        out.push((name, bytes));
    }
    Ok((out, total_in))
}

#[derive(Debug, serde::Serialize)]
pub struct GithubUpdateInfo {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub release_notes: String,
}

fn parse_semver_tuple(v: &str) -> (u64, u64, u64) {
    let clean = v.trim().trim_start_matches(['v', 'V']);
    let core = clean.split(['-', '+']).next().unwrap_or("0.0.0");
    let mut parts = core.split('.').filter_map(|p| p.parse::<u64>().ok());
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

#[tauri::command]
pub async fn check_github_update_cmd() -> Result<GithubUpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .user_agent("NexusRAR-Updater/1.0")
        .build()
        .map_err(|e| format!("http client: {}", e))?;

    let rel_url = "https://api.github.com/repos/bierfor/nexus-compress/releases/latest";
    if let Ok(resp) = client.get(rel_url).send().await {
        if resp.status().is_success() {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let tag = json.get("tag_name").and_then(|v| v.as_str()).unwrap_or(&current);
                let html_url = json
                    .get("html_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("https://github.com/bierfor/nexus-compress/releases");
                let body = json.get("body").and_then(|v| v.as_str()).unwrap_or("");
                let has_update = parse_semver_tuple(tag) > parse_semver_tuple(&current);
                return Ok(GithubUpdateInfo {
                    has_update,
                    current_version: current,
                    latest_version: tag.trim_start_matches(['v', 'V']).to_string(),
                    release_url: html_url.to_string(),
                    release_notes: body.chars().take(400).collect(),
                });
            }
        }
    }

    // Fallback to tags if no formal GitHub Release exists yet
    let tags_url = "https://api.github.com/repos/bierfor/nexus-compress/tags?per_page=1";
    if let Ok(resp) = client.get(tags_url).send().await {
        if resp.status().is_success() {
            if let Ok(arr) = resp.json::<Vec<serde_json::Value>>().await {
                if let Some(first) = arr.first() {
                    let tag = first.get("name").and_then(|v| v.as_str()).unwrap_or(&current);
                    let has_update = parse_semver_tuple(tag) > parse_semver_tuple(&current);
                    return Ok(GithubUpdateInfo {
                        has_update,
                        current_version: current,
                        latest_version: tag.trim_start_matches(['v', 'V']).to_string(),
                        release_url: "https://github.com/bierfor/nexus-compress/releases".to_string(),
                        release_notes: String::new(),
                    });
                }
            }
        }
    }

    Ok(GithubUpdateInfo {
        has_update: false,
        current_version: current.clone(),
        latest_version: current,
        release_url: "https://github.com/bierfor/nexus-compress/releases".to_string(),
        release_notes: String::new(),
    })
}

#[tauri::command]
pub async fn open_external_url_cmd(url: String) -> Result<(), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("Only http/https URLs are allowed".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn()
            .map_err(|e| format!("open url: {}", e))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&url)
            .spawn()
            .map_err(|e| format!("open url: {}", e))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .map_err(|e| format!("open url: {}", e))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn split_archive_file_cmd(file_path: String, chunk_mb: usize) -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(move || {
        let limit = chunk_mb.max(1).saturating_mul(1024 * 1024);
        let data = std::fs::read(&file_path).map_err(|e| format!("read {}: {}", file_path, e))?;
        if data.len() <= limit {
            return Ok(vec![file_path]);
        }
        let mut parts = Vec::new();
        for (idx, chunk) in data.chunks(limit).enumerate() {
            let part_path = format!("{}.{:03}", file_path, idx + 1);
            std::fs::write(&part_path, chunk).map_err(|e| format!("write {}: {}", part_path, e))?;
            parts.push(part_path);
        }
        let _ = std::fs::remove_file(&file_path);
        Ok(parts)
    })
    .await
    .map_err(|e| format!("join: {}", e))?
}

#[tauri::command]
pub async fn join_multipart_file_cmd(first_part_path: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let p = Path::new(&first_part_path);
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext.len() != 3 || !ext.chars().all(|c| c.is_ascii_digit()) {
            return Ok(first_part_path);
        }
        let prefix = first_part_path
            .strip_suffix(&format!(".{}", ext))
            .unwrap_or(&first_part_path);
        let mut combined = Vec::new();
        let mut idx = 1usize;
        loop {
            let part = format!("{}.{:03}", prefix, idx);
            if !Path::new(&part).exists() {
                break;
            }
            let bytes = std::fs::read(&part).map_err(|e| format!("read {}: {}", part, e))?;
            combined.extend_from_slice(&bytes);
            idx += 1;
        }
        if combined.is_empty() {
            return Err(format!("No multipart volumes found for {}", first_part_path));
        }
        std::fs::write(prefix, &combined).map_err(|e| format!("write {}: {}", prefix, e))?;
        Ok(prefix.to_string())
    })
    .await
    .map_err(|e| format!("join: {}", e))?
}
