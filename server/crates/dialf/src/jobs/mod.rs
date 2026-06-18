//! YAML job model + runner.
//!
//! A job is an ordered list of [`schema::Step`]s. The [`runner`] executes them against a
//! [`runner::JobIo`] — the real impl wires audio + a connected phone; the test mock is
//! the hardware-free loopback.

pub mod runner;
pub mod schema;
