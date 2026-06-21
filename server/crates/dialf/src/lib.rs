//! DialF server library.
//!
//! Two planes (see docs/PROTOCOL.md):
//! - **Control plane:** phones connect to `dialfd` over WebSocket ([`protocol`]); the
//!   `dialf` CLI and other tools drive `dialfd` over a local Unix-socket control API.
//! - **Audio plane:** `dialfd` owns the sound card via external CLI tools, runs VAD, and
//!   executes YAML jobs ([`jobs`]).

pub mod audio;
pub mod config;
pub mod daemon;
pub mod hub;
pub mod jobs;
pub mod loopback;
pub mod phone;
pub mod protocol;
pub mod registry;
pub mod service;
pub mod transport;

/// Whether the build is linked against the real ten-vad library (vs. the stub).
/// `audio.wait_for_speech` only works when this is true.
pub fn vad_linked() -> bool {
    ten_vad_sys::is_linked()
}

/// The linked ten-vad version, or `None` in a stub build.
pub fn vad_version() -> Option<String> {
    ten_vad_sys::version()
}
