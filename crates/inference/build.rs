//! Build script for `arknet-inference`.
//!
//! Two jobs:
//!
//! 1. Compile llama.cpp (vendored at `vendor/llama.cpp`, pinned SHA) via
//!    CMake and tell Cargo to link against the resulting static libs.
//! 2. Generate Rust FFI bindings from `include/llama.h` via `bindgen`,
//!    landing them at `$OUT_DIR/bindings.rs` for `src/sys/mod.rs` to
//!    include.
//!
//! # GPU backend selection
//!
//! The GPU backend is selected via the `ARKNET_INFERENCE_GPU`
//! environment variable, **not** via cargo features. Cargo features
//! only gate Rust-level code; the GPU backend gates native toolchain
//! requirements (CUDA SDK, Metal framework, ROCm) that can't be
//! guessed at from the current toolchain alone.
//!
//! Values: `cpu` (default), `cuda`, `metal`, `rocm`, `vulkan`.
//!
//! Examples:
//! - `cargo build -p arknet-inference`                     — CPU (default)
//! - `ARKNET_INFERENCE_GPU=metal cargo build -p arknet-inference`
//! - `ARKNET_INFERENCE_GPU=cuda cargo build -p arknet-inference`

use std::env;
use std::path::{Path, PathBuf};

const GPU_ENV: &str = "ARKNET_INFERENCE_GPU";

fn main() {
    // ── Rebuild triggers ──────────────────────────────────────────────
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest_dir.join("vendor/llama.cpp");
    let header = vendor.join("include/llama.h");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", header.display());
    println!(
        "cargo:rerun-if-changed={}",
        vendor.join("CMakeLists.txt").display()
    );
    println!("cargo:rerun-if-env-changed={GPU_ENV}");

    if !header.exists() {
        panic!(
            "vendored llama.cpp header not found at {}. Did you run `git submodule update --init --recursive`?",
            header.display()
        );
    }

    // ── GPU backend selection ─────────────────────────────────────────
    let backend = match env::var(GPU_ENV).ok().as_deref() {
        None | Some("") | Some("cpu") => GpuBackend::Cpu,
        Some("cuda") => GpuBackend::Cuda,
        Some("metal") => GpuBackend::Metal,
        Some("rocm") => GpuBackend::Rocm,
        Some("vulkan") => GpuBackend::Vulkan,
        Some(other) => panic!(
            "{GPU_ENV}={other} is not recognized. Valid values: cpu | cuda | metal | rocm | vulkan"
        ),
    };

    // ── Compile llama.cpp ─────────────────────────────────────────────
    let dst = build_llama_cpp(&vendor, backend);
    emit_link_flags(&dst, backend);

    // ── Generate FFI bindings ─────────────────────────────────────────
    generate_bindings(&vendor, &header);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum GpuBackend {
    Cpu,
    Cuda,
    Metal,
    Rocm,
    Vulkan,
}

fn build_llama_cpp(vendor: &Path, backend: GpuBackend) -> PathBuf {
    let mut cmake_cfg = cmake::Config::new(vendor);

    // Static libs keep the final binary self-contained; no separate .so
    // files to ship. llama.cpp defaults to shared on some platforms so
    // we flip it explicitly.
    cmake_cfg
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("LLAMA_BUILD_TESTS", "OFF")
        .define("LLAMA_BUILD_EXAMPLES", "OFF")
        .define("LLAMA_BUILD_SERVER", "OFF")
        .define("LLAMA_CURL", "OFF")
        .define("GGML_BUILD_TESTS", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .profile("Release");

    // Explicit defaults so llama.cpp doesn't auto-enable something
    // surprising on the current host (e.g. Metal on macOS CI).
    cmake_cfg
        .define("GGML_METAL", "OFF")
        .define("GGML_CUDA", "OFF")
        .define("GGML_HIPBLAS", "OFF")
        .define("GGML_VULKAN", "OFF");

    match backend {
        GpuBackend::Cpu => {}
        GpuBackend::Cuda => {
            cmake_cfg.define("GGML_CUDA", "ON");
        }
        GpuBackend::Metal => {
            cmake_cfg.define("GGML_METAL", "ON");
        }
        GpuBackend::Rocm => {
            cmake_cfg.define("GGML_HIPBLAS", "ON");
        }
        GpuBackend::Vulkan => {
            cmake_cfg.define("GGML_VULKAN", "ON");
        }
    }

    cmake_cfg.build()
}

fn emit_link_flags(dst: &Path, backend: GpuBackend) {
    // CMake drops archives into `<build>/lib` and `<build>/build/src` etc.
    // We emit a broad search path and name each archive. The set below is
    // what llama.cpp b8951 produces; regenerate if the pinned SHA bumps.
    let lib = dst.join("lib");
    let build64 = dst.join("build").join("src");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-search=native={}", build64.display());
    println!(
        "cargo:rustc-link-search=native={}",
        dst.join("build").join("ggml").join("src").display()
    );

    // Primary libraries. Order matters: link the higher-level ones first
    // so their unresolved symbols are found in the lower-level ones.
    let libs = ["llama", "ggml", "ggml-cpu", "ggml-base"];
    for name in libs {
        println!("cargo:rustc-link-lib=static={name}");
    }

    // Platform-specific system libs llama.cpp / ggml need.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" => {
            println!("cargo:rustc-link-lib=framework=Accelerate");
            println!("cargo:rustc-link-lib=framework=Foundation");
            if backend == GpuBackend::Metal {
                println!("cargo:rustc-link-lib=framework=Metal");
                println!("cargo:rustc-link-lib=framework=MetalKit");
                println!("cargo:rustc-link-lib=framework=MetalPerformanceShaders");
            }
        }
        "linux" => {
            println!("cargo:rustc-link-lib=dylib=m");
            println!("cargo:rustc-link-lib=dylib=pthread");
            println!("cargo:rustc-link-lib=dylib=stdc++");
        }
        _ => {}
    }
}

fn generate_bindings(vendor: &Path, header: &Path) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_path = out_dir.join("bindings.rs");

    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", vendor.join("include").display()))
        .clang_arg(format!("-I{}", vendor.join("ggml/include").display()))
        // Only expose the `llama_*` surface; skip ggml internals, libc, etc.
        .allowlist_function("llama_.*")
        .allowlist_type("llama_.*")
        .allowlist_var("LLAMA_.*")
        // C enums to Rust; rustified enums give us pattern matching + exhaustiveness.
        .rustified_non_exhaustive_enum("llama_.*")
        .derive_default(true)
        .derive_debug(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed to parse llama.h");

    bindings
        .write_to_file(&bindings_path)
        .expect("failed to write bindings.rs");
}
