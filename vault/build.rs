//! The Secure Enclave shim, on a Mac and nowhere else. Every other build, this one
//! included on Linux, skips swiftc entirely and gets a `ThisMachine` that refuses.

use std::env;
use std::path::PathBuf;
use std::process::Command;

const SHIM: &str = "shim/seshim.swift";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={SHIM}");
    // The target's, from cargo: a `cfg!` here would be the host's.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("no Swift target for {other}"),
    };
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    // By its full path: PATH does not choose what compiles the key's gate.
    let status = Command::new("/usr/bin/swiftc")
        .args(["-parse-as-library", "-O", "-whole-module-optimization", "-emit-library", "-static", "-module-name", "seshim"])
        .args(["-target", &format!("{arch}-apple-macos13")])
        .arg("-o")
        .arg(out.join("libseshim.a"))
        .arg(SHIM)
        .status()
        .expect("running /usr/bin/swiftc: the enclave shim needs the Command Line Tools");
    assert!(status.success(), "swiftc failed on {SHIM}");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=seshim");
    for framework in ["CryptoKit", "Foundation", "LocalAuthentication", "Security"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
