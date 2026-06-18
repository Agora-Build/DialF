//! Audio plane: sound-card I/O via external tools, resampling, VAD, and the engine
//! that ties playback/capture/recording together.
//!
//! Present: [`vad`] (turn detection), [`resample`], [`tool_detect`], [`backend`]
//! (trait + WAV file backend), [`command_backend`] (subprocess), and [`engine`].

pub mod backend;
pub mod command_backend;
pub mod engine;
pub mod resample;
pub mod tool_detect;
pub mod vad;
