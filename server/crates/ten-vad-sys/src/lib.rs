//! Safe-ish bindings to the ten-vad C library.
//!
//! The C API (from `include/ten_vad.h`) is small:
//! ```c
//! typedef void *ten_vad_handle_t;
//! int  ten_vad_create (ten_vad_handle_t *handle, size_t hop_size, float threshold);
//! int  ten_vad_process(ten_vad_handle_t handle, const int16_t *audio, size_t len,
//!                      float *out_probability, int *out_flag);
//! int  ten_vad_destroy(ten_vad_handle_t *handle);
//! const char *ten_vad_get_version(void);
//! ```
//!
//! Signatures match the submodule's `include/ten_vad.h` and are exercised by the smoke
//! test in `tests/`. ten-vad is always compiled from source (see `build.rs`), which emits
//! `--cfg ten_vad_linked`; the `not(ten_vad_linked)` stub remains only as a compile
//! fallback (its `TenVad::new` returns `Error::NotLinked`).

use std::fmt;

/// 16 ms hop at 16 kHz — ten-vad's recommended frame size.
pub const HOP_256: usize = 256;

#[derive(Debug)]
pub enum Error {
    /// The crate was built without a linked ten-vad library.
    NotLinked,
    /// A C call returned a non-zero status.
    Native(i32),
    /// Frame length did not match the configured hop size.
    BadFrameLen { expected: usize, got: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotLinked => write!(
                f,
                "ten-vad not linked — build from source (git submodule update --init --recursive)"
            ),
            Error::Native(code) => write!(f, "ten-vad native call failed (code {code})"),
            Error::BadFrameLen { expected, got } => {
                write!(f, "ten-vad frame length mismatch: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Result of processing one hop.
#[derive(Debug, Clone, Copy)]
pub struct VadFrame {
    /// Probability of voice activity in [0.0, 1.0].
    pub probability: f32,
    /// ten-vad's own thresholded voiced/unvoiced decision.
    pub voiced: bool,
}

#[cfg(ten_vad_linked)]
mod ffi {
    use std::os::raw::{c_char, c_int};

    pub type Handle = *mut std::ffi::c_void;

    extern "C" {
        pub fn ten_vad_create(handle: *mut Handle, hop_size: usize, threshold: f32) -> c_int;
        pub fn ten_vad_process(
            handle: Handle,
            audio_data: *const i16,
            audio_data_length: usize,
            out_probability: *mut f32,
            out_flag: *mut c_int,
        ) -> c_int;
        pub fn ten_vad_destroy(handle: *mut Handle) -> c_int;
        pub fn ten_vad_get_version() -> *const c_char;
    }
}

/// A ten-vad instance configured for a fixed hop size.
pub struct TenVad {
    hop_size: usize,
    #[cfg(ten_vad_linked)]
    handle: ffi::Handle,
}

impl TenVad {
    /// Create a VAD expecting `hop_size`-sample i16 frames at 16 kHz.
    /// `threshold` is the voiced decision threshold (e.g. 0.5).
    pub fn new(hop_size: usize, threshold: f32) -> Result<Self, Error> {
        #[cfg(ten_vad_linked)]
        {
            let mut handle: ffi::Handle = std::ptr::null_mut();
            let rc = unsafe { ffi::ten_vad_create(&mut handle, hop_size, threshold) };
            if rc != 0 || handle.is_null() {
                return Err(Error::Native(rc));
            }
            Ok(Self { hop_size, handle })
        }
        #[cfg(not(ten_vad_linked))]
        {
            let _ = (hop_size, threshold);
            Err(Error::NotLinked)
        }
    }

    /// The configured hop size, in samples.
    pub fn hop_size(&self) -> usize {
        self.hop_size
    }

    /// Process one frame of exactly `hop_size` i16 samples (16 kHz mono).
    pub fn process(&mut self, frame: &[i16]) -> Result<VadFrame, Error> {
        if frame.len() != self.hop_size {
            return Err(Error::BadFrameLen {
                expected: self.hop_size,
                got: frame.len(),
            });
        }
        #[cfg(ten_vad_linked)]
        {
            let mut probability: f32 = 0.0;
            let mut flag: std::os::raw::c_int = 0;
            let rc = unsafe {
                ffi::ten_vad_process(
                    self.handle,
                    frame.as_ptr(),
                    frame.len(),
                    &mut probability,
                    &mut flag,
                )
            };
            if rc != 0 {
                return Err(Error::Native(rc));
            }
            Ok(VadFrame {
                probability,
                voiced: flag != 0,
            })
        }
        #[cfg(not(ten_vad_linked))]
        {
            Err(Error::NotLinked)
        }
    }
}

impl Drop for TenVad {
    fn drop(&mut self) {
        #[cfg(ten_vad_linked)]
        unsafe {
            if !self.handle.is_null() {
                ffi::ten_vad_destroy(&mut self.handle);
            }
        }
    }
}

/// Whether this build is linked against the real ten-vad library.
pub const fn is_linked() -> bool {
    cfg!(ten_vad_linked)
}

/// The native ten-vad version string, or `None` in stub builds.
pub fn version() -> Option<String> {
    #[cfg(ten_vad_linked)]
    unsafe {
        let p = ffi::ten_vad_get_version();
        if p.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
    }
    #[cfg(not(ten_vad_linked))]
    {
        None
    }
}
