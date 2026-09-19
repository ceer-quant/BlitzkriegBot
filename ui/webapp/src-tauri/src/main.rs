//! Blitzkrieg desktop shell (Tauri v2) — binary entry. Everything lives in the
//! lib target so the gate can exercise the command seam headlessly
//! (`tests/chain.rs`) against a real core.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    blitzkrieg_webapp::run()
}
