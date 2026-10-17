use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct SystemExecutable {
    pub(super) path: &'static str,
    pub(super) root: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ExecutableIdentities {
    pub(super) git: &'static [SystemExecutable],
    pub(super) shell: SystemExecutable,
    pub(super) git_magics: &'static [&'static [u8]],
    pub(super) program_magics: &'static [&'static [u8]],
    #[cfg(target_os = "macos")]
    pub(super) system_directories: &'static [&'static str],
}

#[cfg(target_os = "linux")]
pub(super) const EXECUTABLES: ExecutableIdentities = ExecutableIdentities {
    git: &[SystemExecutable {
        path: "/usr/bin/git",
        root: "/usr",
    }],
    shell: SystemExecutable {
        path: "/usr/bin/sh",
        root: "/usr",
    },
    git_magics: &[b"\x7fELF"],
    program_magics: &[b"\x7fELF"],
};

#[cfg(target_os = "macos")]
pub(super) const EXECUTABLES: ExecutableIdentities = ExecutableIdentities {
    git: &[SystemExecutable {
        path: "/Library/Developer/CommandLineTools/usr/bin/git",
        root: "/Library/Developer/CommandLineTools",
    }],
    shell: SystemExecutable {
        path: "/bin/sh",
        root: "/bin",
    },
    git_magics: &[b"\xca\xfe\xba\xbe", b"\xcf\xfa\xed\xfe"],
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
};

pub(super) fn git_root(path: &Path) -> bool {
    EXECUTABLES.git.iter().any(|git| path.starts_with(git.root))
}

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
            git: &[SystemExecutable {
                path: "/usr/bin/git",
                root: "/usr",
            }],
            shell: SystemExecutable {
                path: "/usr/bin/sh",
                root: "/usr",
            },
            git_magics: &[b"\x7fELF"],
            program_magics: &[b"\x7fELF"],
        };
        #[cfg(target_os = "macos")]
        let expected = ExecutableIdentities {
            git: &[SystemExecutable {
                path: "/Library/Developer/CommandLineTools/usr/bin/git",
                root: "/Library/Developer/CommandLineTools",
            }],
            shell: SystemExecutable {
                path: "/bin/sh",
                root: "/bin",
            },
            git_magics: &[b"\xca\xfe\xba\xbe", b"\xcf\xfa\xed\xfe"],
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
        };
        assert_eq!(EXECUTABLES, expected);
    }
}
