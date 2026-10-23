fn main() {
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("native.o");
    assert!(std::process::Command::new("cc").args(["-c", "native.c", "-o"]).arg(output).status().unwrap().success());
    println!("cargo:rerun-if-changed=native.c");
}
