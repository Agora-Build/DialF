//! Build script for ten-vad-sys.
//!
//! ten-vad is **always built from source** (the open-source ONNX variant) from the
//! `third_party/ten-vad` submodule — works on any architecture. onnxruntime is
//! auto-provisioned for the target (downloaded from the official releases) unless
//! `ORT_ROOT` is set. The model's hardcoded relative path is patched to honor
//! `TEN_VAD_MODEL` (default = the submodule's model).
//!
//! Emits `--cfg ten_vad_linked`. The `links = "ten_vad"` key plus `cargo:rpath=...`
//! propagate runtime rpath dirs to dependents (see dialf/build.rs).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned onnxruntime version for auto-provisioning.
const ORT_VERSION: &str = "1.17.1";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(ten_vad_linked)");
    println!("cargo:rerun-if-env-changed=TEN_VAD_SRC");
    println!("cargo:rerun-if-env-changed=ORT_ROOT");

    let mut rpaths: Vec<String> = Vec::new();
    build_from_source(&mut rpaths);

    for r in &rpaths {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{r}");
    }
    if !rpaths.is_empty() {
        // Joined with ';' (paths don't contain it); split by dependents.
        println!("cargo:rpath={}", rpaths.join(";"));
    }
}

/// Compile the ONNX variant from the submodule and link onnxruntime.
fn build_from_source(rpaths: &mut Vec<String>) {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_root = match env::var_os("TEN_VAD_SRC") {
        Some(p) => PathBuf::from(p),
        None => manifest.join("../../../third_party/ten-vad"),
    };
    let src_dir = src_root.join("src");
    let inc_dir = src_root.join("include");
    if !src_dir.join("ten_vad.cc").exists() {
        panic!(
            "ten-vad source not found at {} — run `git submodule update --init --recursive` \
             (or set TEN_VAD_SRC)",
            src_dir.display()
        );
    }

    let ort_root = match env::var_os("ORT_ROOT") {
        Some(p) => PathBuf::from(p),
        None => provision_onnxruntime(),
    };
    let ort_inc = ort_root.join("include");
    let ort_lib = ort_root.join("lib");
    if !ort_inc.join("onnxruntime_c_api.h").exists() {
        panic!("onnxruntime_c_api.h not found under {} — check ORT_ROOT", ort_inc.display());
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Absolute default model path; overridable at runtime with $TEN_VAD_MODEL.
    let model_path = fs::canonicalize(src_dir.join("onnx_model/ten-vad.onnx"))
        .unwrap_or_else(|_| src_dir.join("onnx_model/ten-vad.onnx"));
    let model_path = model_path.display().to_string();

    // Patch aed.cc: the model path is a hardcoded relative literal; route it through env.
    let aed = fs::read_to_string(src_dir.join("aed.cc")).expect("read aed.cc");
    let patched = aed.replace(
        "\"onnx_model/ten-vad.onnx\"",
        "(getenv(\"TEN_VAD_MODEL\") ? getenv(\"TEN_VAD_MODEL\") : (char*)TEN_VAD_MODEL_DEFAULT)",
    );
    assert!(
        patched.contains("TEN_VAD_MODEL_DEFAULT"),
        "model-path literal not found in aed.cc; upstream layout changed"
    );
    let patched = format!("#include <stdlib.h>\n{patched}");
    let patched_aed = out_dir.join("aed_patched.cc");
    fs::write(&patched_aed, patched).expect("write patched aed.cc");

    // Collect sources (swap aed.cc for the patched copy).
    let mut cc_files = Vec::new();
    let mut c_files = Vec::new();
    for entry in fs::read_dir(&src_dir).expect("read src dir") {
        let p = entry.unwrap().path();
        match p.extension().and_then(|e| e.to_str()) {
            Some("cc") if p.file_name().unwrap() != "aed.cc" => cc_files.push(p),
            Some("cc") => {} // original aed.cc skipped
            Some("c") => c_files.push(p),
            _ => {}
        }
    }
    cc_files.push(patched_aed);

    println!("cargo:rerun-if-changed={}", src_dir.display());

    let model_def = format!("\"{model_path}\"");

    // C++ translation units.
    let mut cpp = cc::Build::new();
    cpp.cpp(true)
        .include(&src_dir)
        .include(&inc_dir)
        .include(&ort_inc)
        .flag_if_supported("-std=c++14")
        .flag_if_supported("-Wno-write-strings")
        .flag_if_supported("-Wno-unused-result")
        .define("TEN_VAD_MODEL_DEFAULT", model_def.as_str());
    for f in &cc_files {
        cpp.file(f);
    }
    cpp.compile("ten_vad_onnx_cpp");

    // C translation units (e.g. fftw.c).
    if !c_files.is_empty() {
        let mut cbuild = cc::Build::new();
        cbuild
            .include(&src_dir)
            .include(&inc_dir)
            .include(&ort_inc)
            .flag_if_supported("-Wno-unused-result");
        for f in &c_files {
            cbuild.file(f);
        }
        cbuild.compile("ten_vad_onnx_c");
    }

    // Link onnxruntime + the C++ runtime.
    let ort_lib_abs = fs::canonicalize(&ort_lib).unwrap_or(ort_lib);
    let ort_lib_abs = ort_lib_abs.display().to_string();
    println!("cargo:rustc-link-search=native={ort_lib_abs}");
    println!("cargo:rustc-link-lib=dylib=onnxruntime");
    rpaths.push(ort_lib_abs);

    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "macos" {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }

    println!("cargo:rustc-cfg=ten_vad_linked");
    println!("cargo:rustc-env=TEN_VAD_DEFAULT_MODEL={model_path}");
}

/// Download + extract the official onnxruntime release matching the host, into a
/// persistent cache (`$CARGO_HOME/ten-vad-ort/<ver>`). Returns the extracted root.
fn provision_onnxruntime() -> PathBuf {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let (plat, ext) = match (os.as_str(), arch.as_str()) {
        ("macos", "aarch64") => ("osx-arm64", "tgz"),
        ("macos", "x86_64") => ("osx-x86_64", "tgz"),
        ("linux", "x86_64") => ("linux-x64", "tgz"),
        ("linux", "aarch64") => ("linux-aarch64", "tgz"),
        ("windows", "x86_64") => ("win-x64", "zip"),
        _ => panic!(
            "no onnxruntime auto-download mapping for {os}/{arch}; set ORT_ROOT to a manual install"
        ),
    };
    let name = format!("onnxruntime-{plat}-{ORT_VERSION}");
    let asset = format!("{name}.{ext}");

    let cache = cache_dir().join(ORT_VERSION);
    let dest = cache.join(&name);
    if dest.join("include/onnxruntime_c_api.h").exists() {
        return dest; // already provisioned
    }

    fs::create_dir_all(&cache).expect("create ort cache dir");
    let url = format!(
        "https://github.com/microsoft/onnxruntime/releases/download/v{ORT_VERSION}/{asset}"
    );
    let archive = cache.join(&asset);

    println!("cargo:warning=ten-vad-sys: downloading onnxruntime {ORT_VERSION} ({plat}) for from-source build…");
    run("curl", &["-fSL", "-o", path_str(&archive), &url]);
    // bsdtar (macOS/Windows) and GNU tar both extract .tgz; bsdtar also handles .zip.
    run("tar", &["xf", path_str(&archive), "-C", path_str(&cache)]);

    assert!(
        dest.join("include/onnxruntime_c_api.h").exists(),
        "onnxruntime extraction did not produce {} — set ORT_ROOT manually",
        dest.display()
    );
    dest
}

fn cache_dir() -> PathBuf {
    if let Some(h) = env::var_os("CARGO_HOME") {
        return PathBuf::from(h).join("ten-vad-ort");
    }
    if let Some(h) = env::var_os("HOME") {
        return PathBuf::from(h).join(".cargo").join("ten-vad-ort");
    }
    PathBuf::from(env::var("OUT_DIR").unwrap()).join("ten-vad-ort")
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("non-utf8 path")
}

fn run(cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{cmd}`: {e}"));
    if !status.success() {
        panic!("`{cmd} {}` failed with {status}", args.join(" "));
    }
}
