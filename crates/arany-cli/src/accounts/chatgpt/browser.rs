use super::{AuthorizationError, AuthorizationUrl};
use std::{
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const OPEN_DEADLINE: Duration = Duration::from_secs(5);
const LAUNCH_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/snap/bin";
const LAUNCH_ENV: [&str; 13] = [
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_DIRS",
    "XDG_CONFIG_DIRS",
    "XDG_SESSION_TYPE",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_CURRENT_DESKTOP",
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
];

pub(crate) async fn open_authorization_url(
    url: &AuthorizationUrl,
) -> Result<(), AuthorizationError> {
    if url.0.scheme() != "https"
        || url.0.host_str() != Some("auth.openai.com")
        || url.0.port().is_some()
        || url.0.path() != "/api/accounts/authorize"
        || url.0.as_str().len() > 4096
        || url.0.query_pairs().any(|(key, _)| key == "id_token_hint")
    {
        return Err(AuthorizationError::Unavailable);
    }
    let target = url.as_str().to_owned();
    tokio::task::spawn_blocking(move || open_blocking(&target))
        .await
        .map_err(|_| AuthorizationError::Unavailable)?
}

fn open_blocking(target: &str) -> Result<(), AuthorizationError> {
    #[cfg(target_os = "linux")]
    let (program, args): (&str, &[&str]) = if std::path::Path::new("/usr/bin/gio").is_file() {
        ("/usr/bin/gio", &["open"])
    } else {
        ("/usr/bin/xdg-open", &[])
    };
    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) = ("/usr/bin/open", &[]);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(AuthorizationError::Unavailable);

    let mut command = Command::new(program);
    // A cold browser start runs the desktop entry itself, so it needs the display session and a system PATH.
    command
        .args(args)
        .arg(target)
        .env_clear()
        .env("PATH", LAUNCH_PATH)
        .current_dir("/");
    for name in LAUNCH_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = OwnedChild(
        command
            .spawn()
            .map_err(|_| AuthorizationError::Unavailable)?,
    );
    let deadline = Instant::now() + OPEN_DEADLINE;
    loop {
        match child.0.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) | Err(_) => return Err(AuthorizationError::Unavailable),
            Ok(None) if Instant::now() >= deadline => return Err(AuthorizationError::Unavailable),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Url;

    #[test]
    fn cold_browser_start_keeps_the_display_session_without_secrets() {
        for name in [
            "XAUTHORITY",
            "XDG_SESSION_TYPE",
            "XDG_DATA_DIRS",
            "DISPLAY",
            "WAYLAND_DISPLAY",
        ] {
            assert!(LAUNCH_ENV.contains(&name), "{name}");
        }
        assert!(
            LAUNCH_ENV
                .iter()
                .all(|name| !name.contains("KEY") && !name.contains("TOKEN"))
        );
        assert!(
            !LAUNCH_PATH
                .split(':')
                .any(|dir| !dir.starts_with('/') || dir.contains(".."))
        );
    }

    #[tokio::test]
    async fn browser_launch_rejects_token_hints_and_other_destinations_before_a_process() {
        for url in [
            "https://auth.openai.com/api/accounts/authorize?id_token_hint=secret",
            "https://example.com/api/accounts/authorize",
            "http://auth.openai.com/api/accounts/authorize",
        ] {
            assert_eq!(
                open_authorization_url(&AuthorizationUrl(Url::parse(url).unwrap())).await,
                Err(AuthorizationError::Unavailable)
            );
        }
    }
}
