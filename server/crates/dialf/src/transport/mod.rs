//! Transport layer: local control API ([`control_server`]), phone WebSocket plane
//! ([`phone_server`]), and LAN advertisement ([`discovery`]).

pub mod control_server;
pub mod discovery;
pub mod phone_server;
