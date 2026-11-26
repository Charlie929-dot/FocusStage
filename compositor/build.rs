use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/wsl_mask.c");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let out_so = out_dir.join("focusstage-wsl-mask.so");
    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = Command::new(&cc)
        .args([
            "-shared",
            "-fPIC",
            "-O2",
            "-Wall",
            "-Wextra",
            "-o",
        ])
        .arg(&out_so)
        .args(["src/wsl_mask.c", "-ldl"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run {cc} to build src/wsl_mask.c: {e}"));
    assert!(status.success(), "{cc} failed to compile src/wsl_mask.c");
}
