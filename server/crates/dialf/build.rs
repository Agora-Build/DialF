//! Re-emit the ten-vad rpath dirs for this crate's binaries/tests.
//!
//! `ten-vad-sys` links the native lib (framework/dylib) which propagates to our binaries,
//! but `rustc-link-arg` (the rpath) does not propagate. `ten-vad-sys` publishes its rpath
//! dirs as `DEP_TEN_VAD_RPATH` (via its `links` key); we re-emit them here so the `dialf`
//! binary and test executables can locate the lib at runtime.

fn main() {
    println!("cargo:rerun-if-env-changed=DEP_TEN_VAD_RPATH");
    if let Ok(joined) = std::env::var("DEP_TEN_VAD_RPATH") {
        for dir in joined.split(';').filter(|s| !s.is_empty()) {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
        }
    }
}
