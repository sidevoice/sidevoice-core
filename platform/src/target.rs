//! The release platforms shared by the connector runtime, release tooling and desktop installer.
//!
//! This is data, not an OS backend: one table, with names projected from it. The triples are the canonical release
//! build targets (Linux GNU, Windows MSVC), not a claim about the ABI of a caller compiled for the same OS/CPU.
//! [`build_target`] selects by the OS/CPU this binary was compiled for, as the connector runtime does.
//! Actual package availability at a particular version still comes from npm's `optionalDependencies`.

/// One published release platform. Names use Rust's `std::env::consts` except for npm's OS and CPU names.
pub struct ReleaseTarget {
    /// The release archive/inventory name, such as `windows-x86_64`.
    pub name: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    /// The canonical Rust triple used to build releases of this platform.
    pub triple: &'static str,
    pub npm_os: &'static str,
    pub npm_cpu: &'static str,
    /// `.exe` on Windows, empty on macOS and Linux.
    pub exe_suffix: &'static str,
}

/// All released platforms, in the connector tooling's order. Unlisted combinations (including Intel macOS and
/// Windows ARM64) have no release target; do not infer one by independently translating OS and CPU names.
pub const TARGETS: &[ReleaseTarget] = &[
    ReleaseTarget {
        name: "linux-aarch64",
        os: "linux",
        arch: "aarch64",
        triple: "aarch64-unknown-linux-gnu",
        npm_os: "linux",
        npm_cpu: "arm64",
        exe_suffix: "",
    },
    ReleaseTarget {
        name: "linux-x86_64",
        os: "linux",
        arch: "x86_64",
        triple: "x86_64-unknown-linux-gnu",
        npm_os: "linux",
        npm_cpu: "x64",
        exe_suffix: "",
    },
    ReleaseTarget {
        name: "macos-aarch64",
        os: "macos",
        arch: "aarch64",
        triple: "aarch64-apple-darwin",
        npm_os: "darwin",
        npm_cpu: "arm64",
        exe_suffix: "",
    },
    ReleaseTarget {
        name: "windows-x86_64",
        os: "windows",
        arch: "x86_64",
        triple: "x86_64-pc-windows-msvc",
        npm_os: "win32",
        npm_cpu: "x64",
        exe_suffix: ".exe",
    },
];

/// The release platform for Rust's OS and architecture names, or `None` for an unsupported combination.
pub fn for_platform(os: &str, arch: &str) -> Option<&'static ReleaseTarget> {
    TARGETS
        .iter()
        .find(|target| target.os == os && target.arch == arch)
}

/// The release platform for this binary's compiled OS/CPU, not the native machine it runs on.
/// An x64 Windows binary running under emulation on ARM64 Windows returns `windows-x86_64`.
/// This does not probe the native architecture or the compiler's target ABI.
pub fn build_target() -> Option<&'static ReleaseTarget> {
    for_platform(std::env::consts::OS, std::env::consts::ARCH)
}

/// Look up an archive/inventory target name; unknown names are never assigned a default suffix or platform.
pub fn by_name(name: &str) -> Option<&'static ReleaseTarget> {
    TARGETS.iter().find(|target| target.name == name)
}

/// Look up an exact release triple. Other ABIs and glibc-version suffixes are not release triples.
pub fn by_triple(triple: &str) -> Option<&'static ReleaseTarget> {
    TARGETS.iter().find(|target| target.triple == triple)
}

/// A program's file name for this binary's build OS, even when it has no release target. `stem` has no executable suffix.
pub fn exe_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

impl ReleaseTarget {
    /// A program's file name on this release target, independently of the host running the tooling.
    /// `stem` has no executable suffix; it may include an archive-relative directory such as `bin/`.
    pub fn exe_name(&self, stem: &str) -> String {
        format!("{stem}{}", self.exe_suffix)
    }

    /// The connector's npm platform package. Membership at a version is checked in `optionalDependencies`.
    pub fn npm_package(&self) -> String {
        format!("@sidevoice/sidevoice-{}-{}", self.npm_os, self.npm_cpu)
    }

    /// A core or connector archive: `stem-<label>-<target>.tar.zst` when published, `stem-<target>.tar.zst` when
    /// built locally. Callers supply the product stem (`sidevoice-core` or `sidevoice-connector`) and an optional
    /// label (a version without `v`, or `nightly`); product/version selection and validation remain theirs.
    pub fn archive_name(&self, stem: &str, label: Option<&str>) -> String {
        match label {
            Some(label) => format!("{stem}-{label}-{}.tar.zst", self.name),
            None => format!("{stem}-{}.tar.zst", self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_platforms_keep_the_connector_names_and_order() {
        let expected = [
            (
                "linux",
                "aarch64",
                "linux-aarch64",
                "aarch64-unknown-linux-gnu",
                "@sidevoice/sidevoice-linux-arm64",
            ),
            (
                "linux",
                "x86_64",
                "linux-x86_64",
                "x86_64-unknown-linux-gnu",
                "@sidevoice/sidevoice-linux-x64",
            ),
            (
                "macos",
                "aarch64",
                "macos-aarch64",
                "aarch64-apple-darwin",
                "@sidevoice/sidevoice-darwin-arm64",
            ),
            (
                "windows",
                "x86_64",
                "windows-x86_64",
                "x86_64-pc-windows-msvc",
                "@sidevoice/sidevoice-win32-x64",
            ),
        ];
        assert_eq!(TARGETS.len(), expected.len());
        for (target, (os, arch, name, triple, package)) in TARGETS.iter().zip(expected) {
            assert_eq!(target.name, name);
            assert_eq!(for_platform(os, arch).unwrap().name, name);
            assert_eq!(by_name(name).unwrap().triple, triple);
            assert_eq!(by_triple(triple).unwrap().name, name);
            assert_eq!(target.npm_package(), package);
        }
    }

    #[test]
    fn unsupported_combinations_and_abis_are_not_guessed() {
        for (os, arch) in [
            ("macos", "x86_64"),
            ("windows", "aarch64"),
            ("windows", "x86"),
            ("linux", "riscv64"),
            ("linux", "arm"),
            ("freebsd", "x86_64"),
            ("darwin", "arm64"),
            ("", ""),
        ] {
            assert!(for_platform(os, arch).is_none(), "{os}/{arch}");
        }
        assert!(by_name("windows-aarch64").is_none());
        assert!(by_name("unknown").is_none());
        for triple in [
            "x86_64-apple-darwin",
            "aarch64-pc-windows-msvc",
            "x86_64-pc-windows-gnu",
            "x86_64-unknown-linux-musl",
            "x86_64-unknown-linux-gnu.2.28",
            "unknown",
        ] {
            assert!(by_triple(triple).is_none(), "{triple}");
        }
    }

    #[test]
    fn remote_executables_and_archives_do_not_use_the_build_hosts_suffix() {
        let windows = by_triple("x86_64-pc-windows-msvc").unwrap();
        assert_eq!(
            windows.exe_name("bin/sidevoice-connector"),
            "bin/sidevoice-connector.exe"
        );
        assert_eq!(
            windows.exe_name("bin/sidevoice-core"),
            "bin/sidevoice-core.exe"
        );
        assert_eq!(
            windows.exe_name("bin/sidevoice-background"),
            "bin/sidevoice-background.exe"
        );
        assert_eq!(
            windows.archive_name("sidevoice-core", Some("0.10.1")),
            "sidevoice-core-0.10.1-windows-x86_64.tar.zst"
        );
        assert_eq!(
            windows.archive_name("sidevoice-connector", Some("nightly")),
            "sidevoice-connector-nightly-windows-x86_64.tar.zst"
        );
        for name in ["linux-aarch64", "linux-x86_64", "macos-aarch64"] {
            let target = by_name(name).unwrap();
            assert_eq!(
                target.exe_name("bin/sidevoice-connector"),
                "bin/sidevoice-connector"
            );
            assert_eq!(target.exe_name("bin/sidevoice-core"), "bin/sidevoice-core");
        }
        let mac = by_name("macos-aarch64").unwrap();
        assert_eq!(
            mac.archive_name("sidevoice-connector", None),
            "sidevoice-connector-macos-aarch64.tar.zst"
        );
        assert_eq!(
            mac.archive_name("sidevoice-core", Some("0.11.0-rc.1")),
            "sidevoice-core-0.11.0-rc.1-macos-aarch64.tar.zst"
        );
    }

    #[test]
    fn build_target_names_follow_the_compilation_platform() {
        let build = build_target();
        let expected = for_platform(std::env::consts::OS, std::env::consts::ARCH);
        assert_eq!(
            build.map(|target| target.name),
            expected.map(|target| target.name)
        );
        assert_eq!(
            exe_name("sidevoice-connector"),
            format!("sidevoice-connector{}", std::env::consts::EXE_SUFFIX)
        );
        if let Some(build) = build {
            assert_eq!(
                build.exe_name("sidevoice-connector"),
                exe_name("sidevoice-connector")
            );
        }
    }
}
