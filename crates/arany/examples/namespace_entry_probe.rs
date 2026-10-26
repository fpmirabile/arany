#[cfg(target_os = "linux")]
#[path = "namespace_entry/probe.rs"]
mod probe;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    return probe::run();
    #[cfg(not(target_os = "linux"))]
    Err("the namespace entry probe requires Linux".into())
}
