pub mod engine;
pub mod git;
pub mod goals;
#[cfg(unix)]
pub mod process;
pub mod routing;
pub mod scenario;

pub use crate::provider::test_image;

static ACCOUNT_ROOT: std::sync::OnceLock<(std::path::PathBuf, Option<tempfile::TempDir>)> =
    std::sync::OnceLock::new();

pub fn isolate_account_root() -> &'static std::path::Path {
    ACCOUNT_ROOT
        .get_or_init(|| {
            #[cfg(debug_assertions)]
            if let Some(path) = std::env::var_os("ARANY_TEST_ACCOUNT_ROOT") {
                let path = std::path::PathBuf::from(path);
                assert!(path.is_absolute(), "absolute inherited test account root");
                return (path, None);
            }
            let mut builder = tempfile::Builder::new();
            builder.prefix("arany-test-account-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let root = builder.tempdir().expect("private test account root");
            (root.path().to_path_buf(), Some(root))
        })
        .0
        .as_path()
}

pub(crate) fn account_root() -> Option<&'static std::path::Path> {
    ACCOUNT_ROOT.get().map(|(path, _owner)| path.as_path())
}
