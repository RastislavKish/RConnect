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
        // Voice data is compiled with the freshly built espeak-ng, so only natively.
        .define("COMPILE_INTONATIONS", if native { "ON" } else { "OFF" });
    let out = config.build();

    println!(
        "cargo:rustc-link-search=native={}",
        out.join("lib").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        out.join("build/src/ucd-tools").display()
    );
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
