//! Builds eSpeak NG from `mobile/third_party/espeak-ng` as a static C library.
//!
//! Only the C parts are built: no speech-player (C++), no async mode (the speech engine
//! drives eSpeak NG from its own thread), no audio output (the engine plays the samples),
//! no MBROLA and no libsonic.
//!
//! Voice data (`espeak-ng-data`) can only be compiled by an eSpeak NG that runs on the
//! build machine, so it is built for native builds only. The path is exported to the
//! crate as `RCONNECT_ESPEAK_DEV_DATA`, which lets the dev controller speak on Linux.
//! Android builds get the data as app assets, built by `mobile/scripts/build-espeak-data.sh`.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest.join("../third_party/espeak-ng");
    println!("cargo:rerun-if-changed={}", source.join("src").display());
    println!("cargo:rerun-if-changed=build.rs");
    if !source.join("CMakeLists.txt").exists() {
        panic!(
            "eSpeak NG sources are missing in {}; run `git submodule update --init`",
            source.display()
        );
    }

    let native = std::env::var("TARGET").unwrap() == std::env::var("HOST").unwrap();
    let mut config = cmake::Config::new(&source);
    config
        .profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("ENABLE_TESTS", "OFF")
        .define("USE_SPEECHPLAYER", "OFF")
        .define("USE_KLATT", "ON")
        .define("USE_ASYNC", "OFF")
        .define("USE_MBROLA", "OFF")
        .define("USE_LIBSONIC", "OFF")
        .define("USE_LIBPCAUDIO", "OFF")
        .define("ESPEAK_BUILD_MANPAGES", "OFF")
        .define("COMPILE_INTONATIONS", "ON");
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "android" {
        configure_android(&mut config);
    }
    if !native {
        // The default target includes the voice data, which needs an espeak-ng that runs
        // on the build machine. Cross builds build only the library (and ucd with it).
        config.build_target("espeak-ng");
    }
    let out = config.build();

    for dir in ["lib", "build/src/libespeak-ng", "build/src/ucd-tools"] {
        println!("cargo:rustc-link-search=native={}", out.join(dir).display());
    }
    println!("cargo:rustc-link-lib=static=espeak-ng");
    println!("cargo:rustc-link-lib=static=ucd");

    if native {
        let data = out.join("build/espeak-ng-data");
        println!(
            "cargo:rustc-env=RCONNECT_ESPEAK_DEV_DATA={}",
            data.parent().unwrap().display()
        );
    }
}

/// Uses the NDK's own CMake toolchain file, found through the variables cargo-ndk and
/// Android Studio set.
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
