//! Tauri 2.x library entry point for NexusRAR.
//!
//! Sprint 5.7.32 — Android support required splitting the binary
//! into a library + a thin `main.rs`. Tauri 2's mobile build invokes
//! `cargo build --lib` against this crate and then embeds the
//! resulting staticlib in the Gradle project. The desktop build
//! continues to use the `nexus-rar` binary that calls `pub fn run()`.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod archive_inspect;
pub mod commands;
mod db;
pub mod external_archive;
mod p2p_config;
mod p2p_tunnel;
mod throttle;
mod upnp_hole;

use std::sync::Arc;
use tauri::Manager;

/// Public entry point used by both the desktop `main.rs` binary
/// and the Tauri Android runtime. Build the Tauri app, register
/// every IPC command, install the panic-cleanup hook, and run the
/// event loop until the user closes the window.
pub fn run() {
    // Sprint 5.5.3 Phase 2: panic recovery for UPnP port
    // mappings. If the sender process panics while a UPnP hole is
    // open, this hook drains the global registry and removes
    // every mapping it can. Best-effort — SIGKILL bypasses
    // everything, but panic-unwind hits this hook.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[nexus-rar] PANIC: {}", info);
        upnp_hole::emergency_cleanup_all();
        eprintln!("[nexus-rar] emergency UPnP cleanup done");
    }));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Arc::new(commands::P2pState {
            active: Arc::new(tokio::sync::Mutex::new(None)),
        }))
        .manage(Arc::new(tokio::sync::Mutex::new(
            db::open().expect("failed to open local stats db"),
        )))
        .invoke_handler(tauri::generate_handler![
            commands::compress_bytes_cmd,
            commands::decompress_bytes_cmd,
            commands::compress_bytes_with_level_cmd,
            commands::compress_bytes_with_backend_cmd,
            commands::compress_directory_with_backend_cmd,
            commands::compress_target_cmd,
            commands::decompress_target_cmd,
            commands::peek_archive_target_cmd,
            commands::reveal_in_finder_cmd,
            commands::pick_save_location_cmd,
            commands::backend_info_cmd,
            commands::engine_info_cmd,
            commands::self_test_cmd,
            commands::pick_file_cmd,
            commands::pick_files_cmd,
            commands::pick_directory_cmd,
            commands::pick_folders_cmd,
            commands::compress_directory_cmd,
            commands::compress_directories_cmd,
            commands::decompress_directory_cmd,
            commands::peek_archive_cmd,
            commands::peek_archive_file_cmd,
            commands::peek_and_extract_file_cmd,
            commands::read_file_cmd,
            commands::open_path_cmd,
            commands::p2p_send_start_cmd,
            commands::p2p_send_abort_cmd,
            commands::p2p_receive_cmd,
            commands::p2p_receive_direct_cmd,
            commands::p2p_peek_filename_cmd,
            commands::p2p_get_tunnel_config_cmd,
            commands::p2p_save_tunnel_config_cmd,
            commands::p2p_archive_list_cmd,
            commands::p2p_archive_extract_cmd,
            commands::get_stats_cmd,
            commands::get_recent_events_cmd,
            commands::data_dir_cmd,
            commands::reset_stats_cmd,
            commands::unrar_status_cmd,
            commands::compress_to_zip_cmd,
            commands::warmup_cmd,
            commands::check_github_update_cmd,
            commands::open_external_url_cmd,
            commands::split_archive_file_cmd,
            commands::join_multipart_file_cmd,
        ])
        .setup(|app| {
            // Sprint 5.7.31 — explicit window-position fix.
            //
            // Tauri 2.x on Windows has a known issue where
            // frameless windows (decorations=false) sometimes get
            // initialised at the "minimised" coords (-32000, -32000)
            // when the WebView2 renderer hasn't fully attached yet.
            // Force a sane position + size on startup. Tauri's
            // `set_position`/`set_size` setters queue the change
            // on the next event-loop tick, after WebView2 is ready,
            // so it actually lands on screen.
            //
            // Sprint 5.7.32 — guarded by `cfg!(target_os = "windows")`
            // because WebView2 (and the `setup` callback that runs
            // after the WebView is mounted) doesn't apply on
            // Android; the `webview_windows` iterator is empty there.
            if cfg!(target_os = "windows") {
                for (_, win) in app.webview_windows() {
                    if let Some(monitor) = win.primary_monitor().ok().flatten() {
                        let mon = monitor.size();
                        let mon_w = mon.width.max(1200) as i32;
                        let mon_h = mon.height.max(780) as i32;
                        let x = (mon_w - 1200) / 2;
                        let y = (mon_h - 780) / 2;
                        let _ = win.set_position(tauri::PhysicalPosition::new(x.max(0), y.max(0)));
                    } else {
                        let _ = win.set_position(tauri::PhysicalPosition::new(100, 100));
                    }
                    let _ = win.set_size(tauri::PhysicalSize::new(1200, 780));
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running NexusRAR Tauri app");
}