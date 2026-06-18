//! Transport layer.
//!
//! Present: [`control_server`] — the local Unix-socket control API used by `dialf` and
//! other tools. M2 adds `phone_server` (WebSocket) and `discovery` (mDNS).

pub mod control_server;
