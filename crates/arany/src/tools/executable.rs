#[derive(Debug, PartialEq, Eq)]
pub(super) struct SystemExecutable {
    pub(super) path: &'static str,
    pub(super) root: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ExecutableIdentities {
    pub(super) shell: SystemExecutable,
    pub(super) program_magics: &'static [&'static [u8]],
    #[cfg(target_os = "macos")]
    pub(super) system_directories: &'static [&'static str],
    // PATH directories under these roots are never granted whole; programs there need the catalogue.
    pub(super) platform_roots: &'static [&'static str],
}

#[cfg(target_os = "linux")]
pub(super) const EXECUTABLES: ExecutableIdentities = ExecutableIdentities {
    shell: SystemExecutable {
        path: "/usr/bin/sh",
        root: "/usr",
    },
    program_magics: &[b"\x7fELF"],
    platform_roots: &[],
};

#[cfg(target_os = "macos")]
pub(super) const EXECUTABLES: ExecutableIdentities = ExecutableIdentities {
    shell: SystemExecutable {
        path: "/bin/sh",
        root: "/bin",
    },
    program_magics: &[
        b"\xfe\xed\xfa\xce",
        b"\xce\xfa\xed\xfe",
        b"\xfe\xed\xfa\xcf",
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
        b"\xbe\xba\xfe\xca",
        b"\xca\xfe\xba\xbf",
        b"\xbf\xba\xfe\xca",
    ],
    system_directories: &["/bin", "/usr/bin"],
    platform_roots: MACOS_PLATFORM_ROOTS,
};

#[cfg(target_os = "macos")]
const MACOS_PLATFORM_ROOTS: &[&str] = &[
    "/System",
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/libexec",
    "/Library/Apple",
];

pub(super) fn has_magic(bytes: &[u8], magics: &[&[u8]]) -> bool {
    magics.iter().any(|magic| bytes.starts_with(magic))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_executable_trust_is_pinned() {
        #[cfg(target_os = "linux")]
        let expected = ExecutableIdentities {
            shell: SystemExecutable {
                path: "/usr/bin/sh",
                root: "/usr",
            },
            program_magics: &[b"\x7fELF"],
            platform_roots: &[],
        };
        #[cfg(target_os = "macos")]
        let expected = ExecutableIdentities {
            shell: SystemExecutable {
                path: "/bin/sh",
                root: "/bin",
            },
            program_magics: &[
                b"\xfe\xed\xfa\xce",
                b"\xce\xfa\xed\xfe",
                b"\xfe\xed\xfa\xcf",
                b"\xcf\xfa\xed\xfe",
                b"\xca\xfe\xba\xbe",
                b"\xbe\xba\xfe\xca",
                b"\xca\xfe\xba\xbf",
                b"\xbf\xba\xfe\xca",
            ],
            system_directories: &["/bin", "/usr/bin"],
            platform_roots: &[
                "/System",
                "/bin",
                "/sbin",
                "/usr/bin",
                "/usr/sbin",
                "/usr/libexec",
                "/Library/Apple",
            ],
        };
        assert_eq!(EXECUTABLES, expected);
    }
}
