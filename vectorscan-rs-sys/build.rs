use std::path::PathBuf;

#[cfg(not(target_os = "windows"))]
use std::fs::{self, File};
#[cfg(not(target_os = "windows"))]
use std::io::Write;
#[cfg(not(target_os = "windows"))]
use std::path::Path;
#[cfg(not(target_os = "windows"))]
use std::process::Command;

#[cfg(not(target_os = "windows"))]
use nix::{
    errno, mount,
    sched::{unshare, CloneFlags},
    sys::wait,
    unistd,
};

#[cfg(not(target_os = "windows"))]
use anyhow::{Context, Result};

/// Get the environment variable with the given name, panicking if it is not set.
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("`{}` should be set in the environment", name))
}

#[cfg(not(target_os = "windows"))]
fn rename_library(dst: &Path) {
    // Check common output directories: lib and lib64.
    for lib_folder in &[dst.join("lib"), dst.join("lib64")] {
        let hs_path = lib_folder.join("libhs.a");
        let vs_path = lib_folder.join("libvs.a");
        if hs_path.exists() {
            fs::rename(&hs_path, &vs_path).unwrap_or_else(|e| {
                panic!("Failed to rename {:?} to {:?}: {}", hs_path, vs_path, e)
            });
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn setup_environment(out_dir: &Path, target_dir: &Path) -> Result<()> {
    // Get real uid & gid
    let uid = unistd::getuid();
    let gid = unistd::getgid();

    // Unshare user & mount namespaces
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)
        .context("namespace unsharing unsupported")?;

    // Bind mount the output directory to a deterministic target
    mount::mount(
        Some(out_dir),
        target_dir,
        None::<&str>,
        mount::MsFlags::MS_BIND,
        None::<&str>,
    )
    .context("bind mount")?;

    // Remap own uid & gid
    File::create("/proc/self/setgroups")
        .context("create setgroups")?
        .write_all(b"deny")
        .context("deny setgroups")?;
    File::create("/proc/self/uid_map")
        .context("open uid_map")?
        .write_all(format!("{uid} {uid} 1").as_bytes())
        .context("remap uid")?;
    File::create("/proc/self/gid_map")
        .context("open gid_map")?
        .write_all(format!("{gid} {gid} 1").as_bytes())
        .context("remap gid")?;

    Ok(())
}

/// Calls a given callback `f` in an environment where the `out_dir` directory is
/// bind mounted into a deterministic directory
///
/// This allows more reproducible builds, even when the `out_dir` directory is
/// generated.
///
/// In case namespace unsharing is unsupported or blocked, the callback function
/// would be called directly, but with `out_dir` passed to both of its arguments.
#[cfg(not(target_os = "windows"))]
fn run_contained<F>(out_dir: &Path, f: F)
where
    F: Fn(&Path, &Path),
{
    if let unistd::ForkResult::Parent { child } = unsafe { unistd::fork().expect("fork") } {
        loop {
            match wait::waitpid(child, None) {
                Ok(wait::WaitStatus::Exited(_pid, 0)) => return,
                Ok(wait::WaitStatus::Exited(_pid, 1)) => {
                    // Environment setup failed, fall back to standard build
                    eprintln!("Falling back to standard build");
                    f(out_dir, out_dir);
                    return;
                }
                Ok(wait::WaitStatus::Exited(_pid, 101)) => panic!("contained functioned panicked"),
                Ok(_) | Err(errno::Errno::EINTR) => {}
                Err(e) => {
                    panic!("waitpid returned {e:?}");
                }
            }
        }
    }

    // /var is a good target since it's not normally written into, but still
    // exists in almost all systems.
    let target_dir = Path::new("/var");

    if let Err(e) = setup_environment(out_dir, target_dir) {
        eprintln!("Failed to set up environment: {e}");
        unsafe { libc::_exit(1) };
    }

    f(out_dir, target_dir);

    unsafe { libc::_exit(0) };
}

fn main() {
    // Note: use `rerun-if-changed=build.rs` to indicate that this build script *shouldn't* be
    // rerun: see https://doc.rust-lang.org/cargo/reference/build-scripts.html#change-detection
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vectorscan.patch");

    let out_dir = PathBuf::from(env("OUT_DIR"));

    // Choose appropriate C++ runtime library
    #[cfg(not(target_os = "windows"))]
    {
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
        #[cfg(target_os = "windows")]
        panic!("VECTORSCAN_LIB_DIR must be set on Windows to point to the directory containing the prebuilt vectorscan library");

        // In order to trick ccache into thinking the output directory is always
        // the same, try to run the build with a deterministic output directory
        #[cfg(not(target_os = "windows"))]
        {
            const VERSION: &str = "5.4.11";
            let manifest_dir = PathBuf::from(env("CARGO_MANIFEST_DIR"));
            run_contained(&out_dir, move |out_dir, bound_out_dir| {
            let include_dir = bound_out_dir
                .join("include")
                .into_os_string()
                .into_string()
                .unwrap();

            let tarball_path = manifest_dir.join(format!("{VERSION}.tar.gz"));
            let vectorscan_src_dir = bound_out_dir.join(format!("vectorscan-vectorscan-{VERSION}"));

            // Note: patchfile created by diffing pristine extracted release directory tree with modified
            // directory tree, and then running `diff -ruN PRISTINE MODIFIED >PATCHFILE`
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
                // Note: unpack into `out_dir`, giving us the directory at `vectorscan_src_dir`.
                // The downloaded tarball has `vectorscan-vectorscan-{VERSION}` as a prefix on all its entries.
                tar.unpack(bound_out_dir)
                    .expect("Could not unpack Vectorscan source files");
                eprintln!("Tarball extracted to {}", bound_out_dir.display());
            }

            eprintln!(
                "Vectorscan source directory is at {}",
                vectorscan_src_dir.display()
            );

            // Patch release tarball
            {
                let patchfile = File::open(patchfile).expect("Failed to open patchfile");
                let output = Command::new("patch")
                    .args(["-p1"])
                    .current_dir(&vectorscan_src_dir)
                    .stdin(patchfile)
                    .output()
                    .expect("Failed to apply patchfile");
                assert!(output.status.success());
                eprintln!(
                    "Successfully applied patches to Vectorscan source directory at {}",
                    vectorscan_src_dir.display()
                );
            }

            // Build with cmake
            let mut cfg = cmake::Config::new(&vectorscan_src_dir);
            cfg.out_dir(bound_out_dir);

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

            let profile = {
                // See https://doc.rust-lang.org/cargo/reference/profiles.html#opt-level for possible values
                /*
                match env("OPT_LEVEL").as_str() {
                    "0" => "Debug",
                    "s" | "z" => "MinSizeRel",
                    _ => "Release",
                }
                */
                "Release"
            };

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

            cfg.build();
            rename_library(out_dir);

            println!("cargo:rustc-link-search={}", out_dir.join("lib").display());
            println!(
                "cargo:rustc-link-search={}",
                out_dir.join("lib64").display()
            );
        });
        }
    }

    println!("cargo:rustc-link-lib=static=vs");

    // Run hyperscan unit test suite
    #[cfg(feature = "unit_hyperscan")]
    {
        let unittests = out_dir.join("build").join("bin").join("unit-hyperscan");
        match Command::new(unittests).status() {
            Ok(rc) if rc.success() => {}
            Ok(rc) => panic!("Failed to run unit tests: exit with code {rc}"),
            Err(e) => panic!("Failed to run unit tests: {e}"),
        }
    }

    // Run bindgen if needed, or else use the pre-generated bindings
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
        std::fs::copy("src/bindings.rs", out_dir.join("bindings.rs"))
            .expect("Failed to write Rust bindings to Vectorscan");
    }
}
