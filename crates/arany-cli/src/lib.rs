#![deny(unsafe_code)]

pub mod presentation;
pub mod terminal;

#[cfg(all(test, target_os = "macos"))]
use arany::test_support::process as test_process;
