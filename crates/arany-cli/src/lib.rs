#![deny(unsafe_code)]

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Arany supports Linux and macOS; on Windows use WSL2");

pub mod presentation;
pub mod protocol;
pub mod terminal;

#[cfg(all(test, target_os = "macos"))]
use arany::test_support::process as test_process;
