use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // sentry-native dependencies
    match std::env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "macos" => println!("cargo:rustc-link-lib=dylib=c++"),
        "linux" => println!("cargo:rustc-link-lib=dylib=stdc++"),
        _ => return, // allow building with --all-features, fail during runtime
    }

    if !Path::new("sentry-native/.git").exists() {
        let _ = Command::new("git")
            .args(["submodule", "update", "--init", "--recursive"])
            .status();
    }

    let destination = cmake::Config::new("sentry-native")
        // we never need a debug build of sentry-native
        .profile("RelWithDebInfo")
        // always build breakpad regardless of platform defaults
        .define("SENTRY_BACKEND", "breakpad")
        // inject a custom transport instead of curl
        .define("SENTRY_TRANSPORT", "none")
        // build a static library
        .define("BUILD_SHARED_LIBS", "OFF")
        // disable additional targets
        .define("SENTRY_BUILD_TESTS", "OFF")
        .define("SENTRY_BUILD_EXAMPLES", "OFF")
        // cmake sets the install dir to `lib64` on some platforms, since we are not installing to
        // `/usr` or `/usr/local`, we can choose a stable directory for the link-search below.
        .define("CMAKE_INSTALL_LIBDIR", "lib")
        .build();

    println!(
        "cargo:rustc-link-search=native={}",
        destination.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=breakpad_client");
    println!("cargo:rustc-link-lib=static=sentry");

    let bindings = bindgen::Builder::default()
        .header("sentry-native/include/sentry.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("Couldn't write bindings");
}
