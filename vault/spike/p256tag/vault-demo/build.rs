use std::{env, path::PathBuf, process::Command};
fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=shim/seshim.swift");
    let st = Command::new("/usr/bin/swiftc")
        .args(["-parse-as-library", "-O", "-whole-module-optimization", "-emit-library", "-static",
               "-module-name", "seshim", "-target", "arm64-apple-macos13", "-o"])
        .arg(out.join("libseshim.a")).arg("shim/seshim.swift").status().expect("swiftc");
    assert!(st.success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=seshim");
    for f in ["CryptoKit", "Foundation", "LocalAuthentication", "Security"] { println!("cargo:rustc-link-lib=framework={f}"); }
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
