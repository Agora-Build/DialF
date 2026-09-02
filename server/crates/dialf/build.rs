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

    // macOS: embed an Info.plist with the Microphone usage description. tccd wants a usage
    // string before it will show the consent dialog for our explicit request (see
    // audio/mic_permission.rs) — a bare CLI binary has nowhere else to carry one. No
    // CFBundleIdentifier on purpose: TCC keeps keying the grant by binary path.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("Info.plist");
        std::fs::write(
            &plist,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>dialf</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>DialF records call audio from the sound card (or virtual audio device) to run scripted phone calls.</string>
</dict>
</plist>
"#,
        )
        .expect("write embedded Info.plist");
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
