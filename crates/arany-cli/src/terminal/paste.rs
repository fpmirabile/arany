use super::*;

impl AttachedTerminal {
    pub fn paste_into(&mut self, composer: &mut Composer) -> Result<ComposerEdit, &'static str> {
        self.paste_into_workspace(composer, None)
    }

    pub fn paste_into_admitted_workspace(
        &mut self,
        composer: &mut Composer,
        workspace: &std::path::Path,
        identity: arany::WorkspaceIdentity,
    ) -> Result<ComposerEdit, &'static str> {
        self.paste_into_workspace(composer, Some((workspace, identity)))
    }

    fn paste_into_workspace(
        &mut self,
        composer: &mut Composer,
        workspace: Option<(&std::path::Path, arany::WorkspaceIdentity)>,
    ) -> Result<ComposerEdit, &'static str> {
        if let Some(paste) = self.clipboard_paste.take() {
            match paste? {
                ClipboardContent::Image(image) => {
                    composer.attach_image(image)?;
                    self.clipboard_preview = self.is_linear();
                    return Ok(ComposerEdit::Changed);
                }
                ClipboardContent::Text(text) => {
                    self.clipboard_paste = Some(Ok(ClipboardContent::Text(text)));
                }
            }
        }
        let text = self.take_paste()?;
        if let Some((workspace, identity)) = workspace
            && let Some(image) = read_pasted_workspace_image(&text, workspace, identity)?
        {
            composer.attach_image(image)?;
            self.clipboard_preview = self.is_linear();
            return Ok(ComposerEdit::Changed);
        }
        match composer.insert_paste(&text)? {
            ComposerEdit::AtCapacity => Err("paste is too large; paste a smaller section"),
            edit => {
                self.reader()
                    .restore_input_capacity(composer.input_capacity())
                    .expect("admitted Composer input capacity");
                self.clipboard_preview = self.is_linear() && edit == ComposerEdit::Changed;
                Ok(edit)
            }
        }
    }
}

fn read_pasted_workspace_image(
    input: &str,
    workspace: &std::path::Path,
    identity: arany::WorkspaceIdentity,
) -> Result<Option<arany::ImageAttachment>, &'static str> {
    use std::io::Read;
    let Some(path) = pasted_png_path(input, workspace)? else {
        return Ok(None);
    };
    let Some(file) = open_pinned_workspace_png(workspace, &path, identity)? else {
        return Ok(None);
    };
    let initial_capacity = file
        .metadata()
        .map_err(|_| "PNG file is unavailable")?
        .len()
        .min(arany::MAX_IMAGE_BYTES as u64) as usize;
    let mut bytes = Vec::with_capacity(initial_capacity);
    file.take(arany::MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "PNG file could not be read")?;
    if bytes.len() > arany::MAX_IMAGE_BYTES {
        return Err("image is too large; choose a smaller PNG");
    }
    arany::ImageAttachment::from_png(&bytes)
        .map(Some)
        .map_err(|_| "invalid or unsupported PNG image")
}

const MAX_PASTED_PNG_PATH_BYTES: usize = 512;

fn pasted_png_path(
    input: &str,
    workspace: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, &'static str> {
    let path = input.trim().trim_matches(['"', '\'']);
    if path.len() > MAX_PASTED_PNG_PATH_BYTES
        || path.contains(char::is_control)
        || path.contains("://")
        || !path.to_ascii_lowercase().ends_with(".png")
    {
        return Ok(None);
    }
    let pasted_path = std::path::Path::new(path);
    let relative = if pasted_path.is_absolute() {
        let admitted_root = workspace
            .canonicalize()
            .map_err(|_| "Workspace is unavailable")?;
        pasted_path
            .strip_prefix(admitted_root)
            .map_err(|_| "PNG path must be inside the admitted Workspace")?
            .to_str()
            .ok_or("PNG path must be valid UTF-8")?
    } else {
        path.strip_prefix("./").unwrap_or(path)
    };
    let mention = serde_json::to_string(relative).map_err(|_| "invalid PNG path")?;
    let paths = arany::mention_paths(&format!("@{mention}"))
        .map_err(|_| "PNG path must name a file inside the admitted Workspace")?;
    let [relative_path] = paths.as_slice() else {
        return Err("PNG path must name a file inside the admitted Workspace");
    };
    Ok(Some(relative_path.clone()))
}

fn open_pinned_workspace_png(
    workspace: &std::path::Path,
    relative: &std::path::Path,
    identity: arany::WorkspaceIdentity,
) -> Result<Option<std::fs::File>, &'static str> {
    use rustix::fs::{Mode, OFlags, fstat, open};
    use std::os::unix::fs::MetadataExt;

    let workspace = workspace
        .canonicalize()
        .map_err(|_| "Workspace is unavailable")?;
    let root = open(
        workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| "Workspace is unavailable")?;
    let root_stat = fstat(&root).map_err(|_| "Workspace is unavailable")?;
    if identity.device != root_stat.st_dev as u64 || identity.inode != root_stat.st_ino as u64 {
        return Err("Workspace identity changed; PNG was not read");
    }
    let file = open_workspace_png(root, relative, root_stat.st_dev as u64)?;
    let Some(file) = file else {
        return Ok(None);
    };
    let metadata = file.metadata().map_err(|_| "PNG file is unavailable")?;
    if !metadata.is_file() || metadata.dev() != root_stat.st_dev as u64 || metadata.nlink() != 1 {
        return Err("PNG path must be a regular Workspace file without hard links");
    }
    if metadata.len() > arany::MAX_IMAGE_BYTES as u64 {
        return Err("image is too large; choose a smaller PNG");
    }
    Ok(Some(file))
}

fn open_workspace_png(
    root: rustix::fd::OwnedFd,
    relative: &std::path::Path,
    device: u64,
) -> Result<Option<std::fs::File>, &'static str> {
    use rustix::fs::{Mode, OFlags};
    let flags =
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY;
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{ResolveFlags, openat2};
        let _ = device;
        match openat2(
            &root,
            relative,
            flags,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_XDEV,
        ) {
            Ok(file) => Ok(Some(std::fs::File::from(file))),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(_) => Err("PNG path is unavailable or unsafe"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        use rustix::fs::openat;
        use std::os::unix::fs::MetadataExt;
        let relative = relative.to_str().ok_or("PNG path must be valid UTF-8")?;
        let components = relative.split('/').collect::<Vec<_>>();
        let mut file = std::fs::File::from(root);
        for (index, component) in components.iter().enumerate() {
            let flags = if index + 1 < components.len() {
                flags | OFlags::DIRECTORY
            } else {
                flags
            };
            file = match openat(&file, *component, flags, Mode::empty()) {
                Ok(file) => std::fs::File::from(file),
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(_) => return Err("PNG path is unavailable or unsafe"),
            };
            if file
                .metadata()
                .map_err(|_| "PNG path is unavailable")?
                .dev()
                != device
            {
                return Err("PNG path crosses a filesystem boundary");
            }
        }
        Ok(Some(file))
    }
}

#[cfg(test)]
mod workspace_image_tests {
    use super::read_pasted_workspace_image;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::os::unix::fs::MetadataExt;

    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(not(arany_ci), ignore = "native Linux PNG bind-mount gate")]
    fn pasted_png_rejects_same_device_directory_and_file_mounts() {
        use arany::test_support::process::{BoundedOutput, assert_single_test_completed};
        const CHILD: &str = "terminal::workspace_image_tests::mounted_workspace_png_child";
        let _gate = arany::test_support::process::NATIVE_GATE.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(workspace.join("mounted")).unwrap();
        let bytes = STANDARD
            .decode(arany::test_support::test_image().base64())
            .unwrap();
        std::fs::write(outside.join("photo.png"), &bytes).unwrap();
        std::fs::write(workspace.join("photo.png"), &bytes).unwrap();
        std::fs::write(workspace.join("leaf.png"), &bytes).unwrap();
        let output = std::process::Command::new("/usr/bin/bwrap")
            .env_clear()
            .env("ARANY_TEST_PNG_WORKSPACE", &workspace)
            .current_dir(&workspace)
            .args([
                "--unshare-user",
                "--unshare-pid",
                "--unshare-net",
                "--die-with-parent",
                "--ro-bind",
                "/",
                "/",
                "--bind",
            ])
            .arg(&outside)
            .arg(workspace.join("mounted"))
            .arg("--bind")
            .arg(outside.join("photo.png"))
            .arg(workspace.join("leaf.png"))
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .bounded_output()
            .unwrap();
        assert!(
            output.status.success(),
            "mount fixture: {:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_single_test_completed(&output.stdout, CHILD);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "helper-only: activated by the PNG bind-mount gate"]
    fn mounted_workspace_png_child() {
        let workspace =
            std::path::PathBuf::from(std::env::var_os("ARANY_TEST_PNG_WORKSPACE").unwrap());
        let metadata = std::fs::metadata(&workspace).unwrap();
        let identity = arany::WorkspaceIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        assert!(
            read_pasted_workspace_image("photo.png", &workspace, identity)
                .unwrap()
                .is_some()
        );
        for path in ["mounted/photo.png", "mounted/missing.png", "leaf.png"] {
            let mounted = workspace.join(if path == "mounted/missing.png" {
                "mounted"
            } else {
                path
            });
            assert_eq!(std::fs::metadata(mounted).unwrap().dev(), identity.device);
            assert_eq!(
                read_pasted_workspace_image(path, &workspace, identity),
                Err("PNG path is unavailable or unsafe"),
                "{path}"
            );
        }
    }

    #[test]
    fn pasted_png_reads_only_a_pinned_regular_workspace_file() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().canonicalize().unwrap().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let bytes = STANDARD
            .decode(arany::test_support::test_image().base64())
            .unwrap();
        std::fs::create_dir(workspace.join("sub")).unwrap();
        for path in ["photo.png", "sub/photo.png", "PHOTO.PNG"] {
            std::fs::write(workspace.join(path), &bytes).unwrap();
        }
        let metadata = std::fs::metadata(&workspace).unwrap();
        let identity = arany::WorkspaceIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let absolute = workspace.join("photo.png");
        for path in [
            "photo.png",
            "sub/photo.png",
            "PHOTO.PNG",
            absolute.to_str().unwrap(),
        ] {
            let image = read_pasted_workspace_image(path, &workspace, identity)
                .unwrap()
                .unwrap();
            assert_eq!(image.byte_len(), bytes.len());
        }
        assert!(read_pasted_workspace_image("../photo.png", &workspace, identity).is_err());
        assert!(read_pasted_workspace_image("/tmp/photo.png", &workspace, identity).is_err());
        for path in [
            "missing.png",
            "missing/photo.png",
            "Please resize logo.png",
            "https://example.com/a.png",
            "http://example.com/a.PNG",
            "'https://example.com/a.png'",
            "file:///tmp/photo.png",
        ] {
            assert_eq!(
                read_pasted_workspace_image(path, &workspace, identity),
                Ok(None)
            );
        }
        assert!(
            read_pasted_workspace_image(
                "photo.png",
                &workspace,
                arany::WorkspaceIdentity {
                    inode: identity.inode + 1,
                    ..identity
                }
            )
            .is_err()
        );
        std::os::unix::fs::symlink("photo.png", workspace.join("linked.png")).unwrap();
        std::os::unix::fs::symlink("missing.png", workspace.join("dangling.png")).unwrap();
        std::os::unix::fs::symlink("sub", workspace.join("linked-sub")).unwrap();
        std::fs::hard_link(
            workspace.join("sub/photo.png"),
            workspace.join("hard-linked.png"),
        )
        .unwrap();
        for path in [
            "linked.png",
            "dangling.png",
            "linked-sub/photo.png",
            "hard-linked.png",
            "sub/photo.png",
        ] {
            assert!(
                read_pasted_workspace_image(path, &workspace, identity).is_err(),
                "{path}"
            );
        }
        std::fs::write(workspace.join("malformed.png"), b"not a PNG").unwrap();
        assert_eq!(
            read_pasted_workspace_image("malformed.png", &workspace, identity),
            Err("invalid or unsupported PNG image")
        );
        std::fs::File::create(workspace.join("oversized.png"))
            .unwrap()
            .set_len(arany::MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert_eq!(
            read_pasted_workspace_image("oversized.png", &workspace, identity),
            Err("image is too large; choose a smaller PNG")
        );
        let replaced = temp.path().join("replaced-workspace");
        std::fs::rename(&workspace, replaced).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("photo.png"), &bytes).unwrap();
        assert!(read_pasted_workspace_image("photo.png", &workspace, identity).is_err());
        assert!(read_pasted_workspace_image("missing.png", &workspace, identity).is_err());
    }
}
