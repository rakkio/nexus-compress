//! Desktop binary entry point for NexusRAR.
//!
//! Sprint 5.7.32 — this binary is now a thin wrapper around the
//! shared library `run()` so the same code can be reused for the
//! Tauri Android build (which compiles `lib.rs` instead of `main.rs`).
//!
//! The mobile runtime never invokes this file; it links the
//! `nexus_rar` static library produced from `lib.rs`.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

fn main() {
    nexus_rar_lib::run();
}