#[cfg(target_os = "macos")]
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/tools/guard/macos/runtime_compat.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    build_macos_runtime();
}

#[cfg(target_os = "macos")]
fn build_macos_runtime() {
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"))
        .join("arany-runtime-compat.dylib");
    let compiler = cc::Build::new()
        .opt_level(2)
        .warnings(true)
        .warnings_into_errors(true)
        .get_compiler();
    let mut command = compiler.to_command();
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") {
        command.args(["-arch", "arm64", "-arch", "arm64e"]);
    }
    let status = command
        .arg("-dynamiclib")
        .arg("src/tools/guard/macos/runtime_compat.c")
        .arg("-o")
        .arg(&output)
        .status()
        .expect("the macOS runtime helper requires the selected C compiler");
    assert!(status.success(), "macOS runtime helper compilation failed");
    let size = std::fs::metadata(output)
        .expect("the compiler produced the macOS runtime helper")
        .len();
    assert!(
        size > 0 && size <= 128 * 1024,
        "macOS runtime helper exceeds 128 KiB"
    );
}

#[cfg(not(target_os = "macos"))]
fn build_macos_runtime() {
    panic!("building the macOS runtime helper requires a macOS build host");
}
