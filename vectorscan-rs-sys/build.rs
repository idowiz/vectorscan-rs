use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Get the environment variable with the given name, panicking if it is not set.
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("`{}` should be set in the environment", name))
}

fn rename_library(dst: &Path) {
    for lib_folder in &[dst.join("lib"), dst.join("lib64")] {
        let src = lib_folder.join("libhs.a");
        let dest = lib_folder.join("libvs.a");
        if src.exists() {
            fs::rename(&src, &dest).unwrap_or_else(|e| {
                panic!("Failed to rename {:?} to {:?}: {}", src, dest, e)
            });
        }
    }
}

fn build_vectorscan(manifest_dir: &Path, out_dir: &Path, is_windows_gnu: bool) {
    let include_dir = out_dir
        .join("include")
        .into_os_string()
        .into_string()
        .unwrap();

    let tarball_path = manifest_dir.join("5.4.12.tar.gz");
    let vectorscan_src_dir = out_dir.join("vectorscan-5.4.12");

    // patch generated with `git diff --no-index`, applied with -p2
    let patchfile = manifest_dir.join("vectorscan.patch");

    // Extract release tarball
    {
        match std::fs::remove_dir_all(&vectorscan_src_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("Failed to clean Vectorscan source directory: {e}"),
        }
        let infile =
            File::open(tarball_path).expect("Failed to open Vectorscan release tarball");
        let gz = flate2::read::GzDecoder::new(infile);
        let mut tar = tar::Archive::new(gz);
        for entry in tar.entries().expect("Failed to read tarball entries") {
            let mut entry = entry.expect("Failed to read tarball entry");
            let path = entry.path().expect("Failed to read entry path").into_owned();
            let stripped: PathBuf = path.components().skip(1).collect();
            if stripped.as_os_str().is_empty() {
                continue;
            }
            let dest = vectorscan_src_dir.join(&stripped);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).expect("Failed to create parent directory");
            }
            entry.unpack(&dest).expect("Failed to unpack tarball entry");
        }
        eprintln!("Tarball extracted to {}", vectorscan_src_dir.display());
    }

    eprintln!(
        "Vectorscan source directory is at {}",
        vectorscan_src_dir.display()
    );

    // Patch release tarball
    {
        let patchfile = File::open(&patchfile).expect("Failed to open patchfile");
        let output = Command::new("patch")
            .arg("-p2")
            .current_dir(&vectorscan_src_dir)
            .stdin(patchfile)
            .output()
            .expect("Failed to apply patchfile");
        assert!(
            output.status.success(),
            "Failed to apply patch: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!(
            "Successfully applied patches to Vectorscan source directory at {}",
            vectorscan_src_dir.display()
        );
    }

    // Build with cmake
    let mut cfg = cmake::Config::new(&vectorscan_src_dir);
    cfg.out_dir(out_dir);

    macro_rules! cfg_define_feature {
        ($cmake_feature: tt, $cargo_feature: tt) => {
            cfg.define(
                $cmake_feature,
                if cfg!(feature = $cargo_feature) {
                    "ON"
                } else {
                    "OFF"
                },
            )
        };
    }

    let profile = if is_windows_gnu { "RelWithDebInfo" } else { "Release" };
    cfg.profile(profile)
        .define("CMAKE_INSTALL_INCLUDEDIR", &include_dir)
        .define("CMAKE_VERBOSE_MAKEFILE", "ON")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_STATIC_LIBS", "ON")
        .define("WARNINGS_AS_ERRORS", "OFF")
        .define("BUILD_EXAMPLES", "OFF")
        .define("BUILD_BENCHMARKS", "OFF")
        .define("BUILD_DOC", "OFF")
        .define("BUILD_TOOLS", "OFF");

    cfg_define_feature!("BUILD_UNIT", "unit_hyperscan");
    cfg_define_feature!("USE_CPU_NAIVE", "cpu_native");

    if cfg!(feature = "asan") {
        cfg.define("SANITIZE", "address");
    }

    if cfg!(feature = "fat_runtime") {
        cfg.define("FAT_RUNTIME", "ON");
    } else {
        cfg.define("FAT_RUNTIME", "OFF");
    }

    // NOTE: Several Vectorscan feature flags can be set based on available CPU SIMD features.
    // Enabling these according to availability on the build system CPU is fragile, however:
    // the resulting binary will not work correctly on machines with CPUs with different SIMD
    // support.
    //
    // By default, we simply disable these options. However, using the `simd-specialization`
    // feature flag, these Vectorscan features will be enabled if the build system's CPU
    // supports them.
    //
    // See
    // https://doc.rust-lang.org/reference/attributes/codegen.html#the-target_feature-attribute
    // for supported target_feature values.

    if cfg!(feature = "simd_specialization") {
        macro_rules! x86_64_feature {
            () => {{
                #[cfg(target_arch = "x86_64")]
                {
                    "ON"
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    "OFF"
                }
            }};
        }

        macro_rules! aarch64_feature {
            () => {{
                #[cfg(target_arch = "aarch64")]
                {
                    "ON"
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    "OFF"
                }
            }};
        }

        cfg.define("BUILD_AVX2", x86_64_feature!());
        // XXX use avx512vbmi as a proxy for this, as it's not clear which particular avx512
        // instructions are needed
        cfg.define("BUILD_AVX512", x86_64_feature!());
        cfg.define("BUILD_AVX512VBMI", x86_64_feature!());

        cfg.define("BUILD_SVE", aarch64_feature!());
        cfg.define("BUILD_SVE2", aarch64_feature!());
        cfg.define("BUILD_SVE2_BITPERM", aarch64_feature!());
    } else {
        cfg.define("BUILD_AVX2", "OFF")
            .define("BUILD_AVX512", "OFF")
            .define("BUILD_AVX512VBMI", "OFF")
            .define("BUILD_SVE", "OFF")
            .define("BUILD_SVE2", "OFF")
            .define("BUILD_SVE2_BITPERM", "OFF");
    }

    if is_windows_gnu {
        cfg.define("GNUCC_ARCH", "x86-64");
        cfg.define("TUNE_FLAG", "generic");
        cfg.cflag("-Wno-narrowing");
        cfg.cxxflag("-Wno-narrowing");

        if cfg!(feature = "fat_runtime") {
            let libc_path = String::from_utf8(
                Command::new("gcc")
                    .args(["--print-file-name=libmsvcrt.a"])
                    .output()
                    .expect("Failed to get libmsvcrt.a path from gcc")
                    .stdout,
            )
            .expect("Invalid UTF-8 in gcc output")
            .trim()
            .to_string();
            std::env::set_var("VECTORSCAN_LIBC_SO", &libc_path);
            std::env::set_var("NM", "nm");
            std::env::set_var("OBJCOPY", "objcopy");
            std::env::set_var("OBJDUMP", "objdump");
        }
    }

    cfg.build();

    rename_library(out_dir);

    println!("cargo:rustc-link-search={}", out_dir.join("lib").display());
    println!(
        "cargo:rustc-link-search={}",
        out_dir.join("lib64").display()
    );
}

fn main() {
    let target_os = env("CARGO_CFG_TARGET_OS");
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let is_windows_gnu = target_os == "windows" && target_env == "gnu";
    let is_windows_msvc = target_os == "windows" && target_env == "msvc";


    println!("cargo:rerun-if-env-changed=VECTORSCAN_LIB_DIR");

    // Note: use `rerun-if-changed=build.rs` to indicate that this build script *shouldn't* be
    // rerun: see https://doc.rust-lang.org/cargo/reference/build-scripts.html#change-detection
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vectorscan.patch");

    // Features affect cmake configuration, so the build script must re-run when they change.
    // CARGO_FEATURE_* env vars are set by cargo when features are enabled.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_FAT_RUNTIME");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SIMD_SPECIALIZATION");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_CPU_NATIVE");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_UNIT_HYPERSCAN");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_ASAN");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_WHOLE_ARCHIVE");

    let manifest_dir = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env("OUT_DIR"));

    if is_windows_msvc {
        let lib_dir = std::env::var("VECTORSCAN_LIB_DIR").expect(
            "VECTORSCAN_LIB_DIR must be set for MSVC targets, \
             pointing to a directory containing vs.lib (import library for vs.dll)",
        );
        println!("cargo:rustc-link-search={lib_dir}");
        println!("cargo:rustc-link-lib=dylib=vs");
    } else {
        if is_windows_gnu {
            println!("cargo:rustc-link-lib=stdc++");
        } else {
            let compiler_version_out = String::from_utf8(
                Command::new("c++")
                    .args(["-v"])
                    .output()
                    .expect("Failed to get C++ compiler version")
                    .stderr,
            )
            .unwrap();

            if compiler_version_out.contains("gcc") {
                println!("cargo:rustc-link-lib=stdc++");
            } else if compiler_version_out.contains("clang") {
                println!("cargo:rustc-link-lib=c++");
            } else {
                panic!("No compatible compiler found: either clang or gcc is needed");
            }
        }

        if let Some(lib_dir) = std::env::var_os("VECTORSCAN_LIB_DIR") {
            println!("cargo:rustc-link-search={}", lib_dir.display());
        } else {
            build_vectorscan(&manifest_dir, &out_dir, is_windows_gnu);
        }

        if cfg!(feature = "whole_archive") {
            println!("cargo:rustc-link-lib=static:+whole-archive=vs");
        } else {
            println!("cargo:rustc-link-lib=static=vs");
        }

        #[cfg(feature = "unit_hyperscan")]
        {
            let unittests = out_dir.join("build").join("bin").join("unit-hyperscan");
            match Command::new(unittests).status() {
                Ok(rc) if rc.success() => {}
                Ok(rc) => panic!("Failed to run unit tests: exit with code {rc}"),
                Err(e) => panic!("Failed to run unit tests: {e}"),
            }
        }
    }

    #[cfg(feature = "bindgen")]
    {
        let config = bindgen::Builder::default()
            .allowlist_function("hs_.*")
            .allowlist_type("hs_.*")
            .allowlist_var("HS_.*")
            .header("wrapper.h")
            .clang_arg(format!("-I{}", &include_dir));
        config
            .generate()
            .expect("Unable to generate bindings")
            .write_to_file(out_dir.join("bindings.rs"))
            .expect("Failed to write Rust bindings to Vectorscan");
    }
    #[cfg(not(feature = "bindgen"))]
    {
        fs::copy("src/bindings.rs", out_dir.join("bindings.rs"))
            .expect("Failed to write Rust bindings to Vectorscan");
    }
}
