//! YAML job model + runner.
//!
//! A job is an ordered list of [`schema::Step`]s. The [`runner`] executes them against a
//! [`runner::JobIo`] — the real impl wires audio + a connected phone; unit tests use an
//! in-test mock.

pub mod runner;
pub mod schema;
