//! Links libopus. On Linux hosts the system library is used, found through pkg-config.
//! Android builds, and hosts without the library, build the vendored copy in
//! `mobile/third_party/opus` as a static library, without network access.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let android = std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "android";
    if !android && pkg_config::Config::new().probe("opus").is_ok() {
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest.join("../mobile/third_party/opus");
    println!("cargo:rerun-if-changed={}", source.join("src").display());
    if !source.join("CMakeLists.txt").exists() {
        panic!(
            "libopus sources are missing in {}; run `git submodule update --init`",
            source.display()
        );
    }
    let mut config = cmake::Config::new(&source);
    config
        .profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("OPUS_BUILD_SHARED_LIBRARY", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_INSTALL_PKG_CONFIG_MODULE", "OFF")
        .define("OPUS_INSTALL_CMAKE_CONFIG_MODULE", "OFF");
    if android {
        configure_android(&mut config);
    }
    let out = config.build();
    println!(
        "cargo:rustc-link-search=native={}",
        out.join("lib").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        out.join("lib64").display()
    );
    println!("cargo:rustc-link-lib=static=opus");
}

/// Uses the NDK's own CMake toolchain file, like `mobile/core/build.rs` does for eSpeak NG.
fn configure_android(config: &mut cmake::Config) {
    let ndk = [
        "ANDROID_NDK_HOME",
        "ANDROID_NDK_ROOT",
        "ANDROID_NDK",
        "NDK_HOME",
    ]
    .iter()
    .find_map(std::env::var_os)
    .map(PathBuf::from)
    .expect("set ANDROID_NDK_HOME to the Android NDK");
    let abi = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64-v8a",
        "arm" => "armeabi-v7a",
        "x86_64" => "x86_64",
        "x86" => "x86",
        other => panic!("unsupported Android architecture {other}"),
    };
    let api = std::env::var("CARGO_NDK_ANDROID_PLATFORM")
        .or_else(|_| std::env::var("ANDROID_PLATFORM"))
        .unwrap_or_else(|_| "28".into());
    let api = api.trim_start_matches("android-");
    config
        .define(
            "CMAKE_TOOLCHAIN_FILE",
            ndk.join("build/cmake/android.toolchain.cmake"),
        )
        .define("ANDROID_ABI", abi)
        .define("ANDROID_PLATFORM", format!("android-{api}"));
    for name in [
        "ANDROID_NDK_HOME",
        "ANDROID_NDK_ROOT",
        "ANDROID_NDK",
        "NDK_HOME",
        "ANDROID_PLATFORM",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
}
