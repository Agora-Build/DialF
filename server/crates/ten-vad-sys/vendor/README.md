# Vendoring the ten-vad native library (for the `prebuilt` feature)

By default `ten-vad-sys` **builds ten-vad from source** (the `third_party/ten-vad`
submodule + onnxruntime) — no vendoring needed. This directory is only used when you opt
into the **`prebuilt`** feature (`cargo build --features prebuilt`), which links a prebuilt
ten-vad lib instead. To use it:

1. Get the prebuilt lib + header from https://github.com/TEN-framework/ten-vad
   (`git clone` or download the release). The repo ships:
   - `lib/Linux/x64/libten_vad.so`
   - `lib/macOS/ten_vad.framework`   (universal arm64 + x86_64)
   - `include/ten_vad.h`

2. Put the platform library where `build.rs` looks (default: `vendor/lib/`), e.g.
   - Linux: copy `libten_vad.so` into `vendor/lib/`
   - macOS: copy `ten_vad.framework` into `vendor/lib/`

   …or set `TEN_VAD_LIB_DIR=/path/to/lib` when building.

3. Ensure the loader can find it at runtime:
   - Linux: `LD_LIBRARY_PATH=vendor/lib` (or install the `.so` system-wide / set rpath)
   - macOS: framework search path / `DYLD_FRAMEWORK_PATH`

4. Rebuild. `build.rs` emits `--cfg ten_vad_linked` and the FFI path is compiled in.

## Verify the C signatures
Before trusting the link, diff the `extern "C"` block in `src/lib.rs` against the actual
`include/ten_vad.h`. The bindings assume:

```c
typedef void *ten_vad_handle_t;
int  ten_vad_create (ten_vad_handle_t *handle, size_t hop_size, float threshold);
int  ten_vad_process(ten_vad_handle_t handle, const int16_t *audio, size_t len,
                     float *out_probability, int *out_flag);
int  ten_vad_destroy(ten_vad_handle_t *handle);
const char *ten_vad_get_version(void);
```

If the header differs, update `src/lib.rs` accordingly.
