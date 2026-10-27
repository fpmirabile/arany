use super::*;

const XATTR_BYTES: usize = 64 * 1024;
const UUID_BYTES: usize = 16;

pub(super) fn validate(file: &File, budget: &mut CommandScanBudget) -> Result<bool, ToolError> {
    let path = format!("/proc/self/fd/{}", file.as_raw_fd());
    let mut names = vec![0; XATTR_BYTES];
    let count = rustix::fs::listxattr(&path, &mut names).map_err(|_| ToolError::Path)?;
    names.truncate(count);
    charge(budget, count)?;
    if count == 0 {
        return Ok(false);
    }
    let mut opaque = false;
    let mut value = vec![0; XATTR_BYTES];
    for name in names.split_inclusive(|byte| *byte == 0) {
        let name = std::ffi::CStr::from_bytes_with_nul(name).map_err(|_| ToolError::Path)?;
        let count = rustix::fs::getxattr(&path, name, &mut value).map_err(|_| ToolError::Path)?;
        charge(budget, count)?;
        let name = name.to_bytes();
        opaque |= classify(
            name,
            &value[..count],
            file.metadata().map_err(|_| ToolError::Path)?.is_dir(),
        )?;
    }
    Ok(opaque)
}

fn classify(name: &[u8], value: &[u8], directory: bool) -> Result<bool, ToolError> {
    let Some(private) = name
        .strip_prefix(b"user.overlay.")
        .or_else(|| name.strip_prefix(b"trusted.overlay."))
    else {
        return Ok(false);
    };
    if !supported(private, value)
        || matches!(private, b"opaque" | b"impure" | b"uuid") && !directory
    {
        return Err(ToolError::Path);
    }
    Ok(private == b"opaque")
}

fn charge(budget: &mut CommandScanBudget, bytes: usize) -> Result<(), ToolError> {
    budget.path_metadata_bytes = budget
        .path_metadata_bytes
        .checked_add(bytes)
        .ok_or(ToolError::Limit)?;
    if budget.path_metadata_bytes > COMMAND_PATH_METADATA_BYTES {
        return Err(ToolError::Limit);
    }
    Ok(())
}

fn supported(name: &[u8], value: &[u8]) -> bool {
    match name {
        b"opaque" | b"impure" => value == b"y",
        b"uuid" => value.len() == UUID_BYTES,
        b"origin" => value.is_empty() || valid_origin(value),
        _ => false,
    }
}

fn valid_origin(value: &[u8]) -> bool {
    const HEADER_BYTES: usize = 21;
    value.len() > HEADER_BYTES
        && value[0] == 0
        && value[1] == 0xfb
        && usize::from(value[2]) == value.len()
        && value[3] & !0x07 == 0
        && value[4] != 0
        && value[4] != u8::MAX
        && (value.len() - HEADER_BYTES).is_multiple_of(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_metadata_is_a_closed_bounded_format() {
        for (name, value, valid) in [
            (b"opaque".as_slice(), b"y".as_slice(), true),
            (b"opaque", b"x", false),
            (b"whiteout", b"", false),
            (b"redirect", b"target", false),
            (b"metacopy", b"y", false),
            (b"impure", b"y", true),
            (b"impure", b"yes", false),
            (b"origin", b"", true),
            (b"origin", b"malformed", false),
            (b"uuid", &[0; UUID_BYTES], true),
            (b"uuid", &[0; UUID_BYTES + 1], false),
        ] {
            let name = [b"user.overlay.".as_slice(), name].concat();
            assert_eq!(classify(&name, value, true).is_ok(), valid, "{name:?}");
        }
        for (name, value) in [
            (
                b"security.selinux".as_slice(),
                b"system_u:object_r:tmp_t:s0".as_slice(),
            ),
            (b"system.posix_acl_access", b"bounded ACL bytes"),
            (b"system.posix_acl_default", b"bounded ACL bytes"),
            (b"user.note", b"ordinary"),
        ] {
            assert!(!classify(name, value, false).unwrap());
        }
        assert!(classify(b"user.overlay.opaque", b"y", false).is_err());
    }
    #[test]
    fn xattr_values_are_charged_to_the_aggregate_metadata_bound() {
        let root = tempfile::tempdir().unwrap();
        let file = File::create(root.path().join("file")).unwrap();
        rustix::fs::fsetxattr(
            &file,
            "user.note",
            b"metadata",
            rustix::fs::XattrFlags::CREATE,
        )
        .unwrap();
        let mut budget = CommandScanBudget {
            path_metadata_bytes: COMMAND_PATH_METADATA_BYTES - b"user.note\0".len(),
            ..Default::default()
        };
        assert!(matches!(
            validate(&file, &mut budget),
            Err(ToolError::Limit)
        ));
    }
}
