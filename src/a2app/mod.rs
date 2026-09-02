//! Splash mini-app support (the `a2app` feature): AI-generated sandboxed
//! mini-apps that run in their own isolates, managed from the Mini Apps
//! screen and shareable into rooms.
//!
//! The launcher-independent logic lives in the `a2app-core` and
//! `a2app-agent` crates; this module is the Robrix-side UI and glue.

use makepad_widgets::*;

/// AI agent sessions and the MCP tool bridge (per-AI-room sessions whose
/// models call host tools like `launch_splash_app` / `send_message`).
pub mod ai;

/// The modal pane that hosts running mini-apps.
pub mod host_pane;
/// Shared host widgets used by every host surface.
pub mod host_set;
/// The registry of live (app, room) instances that surfaces adopt and release.
pub mod instances;
/// The Mini Apps management screen (a top-level navigation tab page).
pub mod mini_apps_screen;
/// The runtime permission prompt modal.
pub mod permission_prompt;
/// The in-room mini-app pane docked beside a room's timeline.
pub mod dock;
pub mod tab_screen;
/// State ownership and per-event-pass processing.
pub mod runtime;
/// The matrix services a mini-app can call, run on the worker.
pub mod matrix;
/// Worker-side room watches that feed the mini-app hooks.
pub mod room_watch;
/// The worker-side account watch behind the rooms-list hooks.
pub mod account_watch;
/// The timeline card for `rs.robius.a2app` events (apps shared into rooms).
pub mod timeline_card;

pub fn script_mod(vm: &mut ScriptVm) {
    host_set::script_mod(vm);
    permission_prompt::script_mod(vm);
    host_pane::script_mod(vm);
    dock::script_mod(vm);
    tab_screen::script_mod(vm);
    timeline_card::script_mod(vm);
    mini_apps_screen::script_mod(vm);
}
