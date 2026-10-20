use crate::process::BoundedOutput;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) struct TlsFixture {
    pub shim: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca: PathBuf,
}

pub(crate) fn build(root: &Path) -> TlsFixture {
    let shim = root.join("loopback.so");
    assert!(
        Command::new("/usr/bin/gcc")
            .args(["-Wall", "-Wextra", "-Werror", "-shared", "-fPIC"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-tls-loopback.c"))
            .args(["-ldl", "-o"])
            .arg(&shim)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    let cert = root.join("server.pem");
    let key = root.join("server.key");
    let ca = root.join("ca.pem");
    let ca_key = root.join("ca.key");
    let csr = root.join("server.csr");
    assert!(
        Command::new("/usr/bin/openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=arany-native-tool-fixture",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-keyout"
            ])
            .arg(&ca_key)
            .arg("-out")
            .arg(&ca)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("/usr/bin/openssl")
            .args([
                "req",
                "-new",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-subj",
                "/CN=api.openai.com",
                "-addext",
                "subjectAltName=DNS:api.openai.com,DNS:api.anthropic.com",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "extendedKeyUsage=serverAuth",
                "-keyout"
            ])
            .arg(&key)
            .arg("-out")
            .arg(&csr)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("/usr/bin/openssl")
            .args(["x509", "-req", "-in"])
            .arg(&csr)
            .arg("-CA")
            .arg(&ca)
            .arg("-CAkey")
            .arg(&ca_key)
            .args([
                "-CAcreateserial",
                "-days",
                "1",
                "-copy_extensions",
                "copy",
                "-out"
            ])
            .arg(&cert)
            .env_clear()
            .bounded_output()
            .unwrap()
            .status
            .success()
    );
    TlsFixture {
        shim,
        cert,
        key,
        ca,
    }
}
