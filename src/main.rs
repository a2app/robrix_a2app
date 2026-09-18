// this stub is necessary because some platforms require building
// as dll (mobile / wasm) and some require to be built as executable
// unfortunately cargo doesn't facilitate this without a main.rs stub

// Hide the command prompt console window on Windows, if desired.
// TODO: move this into Makepad itself as an addition to the `MAKEPAD` env var.
#![cfg_attr(
    all(any(feature = "hide_windows_console", packaging_build), target_os = "windows"),
    windows_subsystem = "windows",
)]

fn main() {
    // Headless MCP tool-bridge mode: when an agent spawns Robrix as its MCP
    // server (`robrix --mcp-bridge --socket <path>`), relay the agent's MCP
    // stdio to the real Robrix process and exit — no window, no makepad.
    #[cfg(all(feature = "a2app", unix))]
    if let Some(code) = robrix::a2app::ai::bridge::maybe_run() {
        std::process::exit(code);
    }
    robrix::app::app_main()
}
