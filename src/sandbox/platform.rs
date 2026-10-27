//! Platform runtime paths required for ordinary executables to start.
//!
//! These are compatibility grants, not workspace policy. Keep this module in
//! sync with the embedded Nono release when upgrading it.

use std::path::PathBuf;

pub(crate) fn command_path() -> String {
    let entries = [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/home/linuxbrew/.linuxbrew/bin",
        "/home/linuxbrew/.linuxbrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/bin",
        "/sbin",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|entry| entry.is_dir());
    std::env::join_paths(entries)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

#[cfg(target_os = "macos")]
pub(crate) fn read_paths() -> &'static [&'static str] {
    &[
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/usr/lib",
        "/usr/local/lib",
        "/usr/share",
        "/System/Library",
        "/Library",
        "/dev",
        "/private/var/db",
        "/private/etc/ssl",
        "/etc/ssl",
        "/etc",
        "/private/etc",
        "/usr/share/zoneinfo",
        "/usr/share/locale",
        "/var/db/timezone",
        "/usr/share/terminfo",
        "/var",
        "/private/var",
        "/private",
        "/tmp",
        "/System/Volumes",
        "/System/Cryptexes",
        "/Applications",
        "/cores",
        "/opt",
        "/run",
        "/nix",
    ]
}

#[cfg(target_os = "linux")]
pub(crate) fn read_paths() -> &'static [&'static str] {
    &[
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/lib",
        "/lib64",
        "/lib/x86_64-linux-gnu",
        "/lib/aarch64-linux-gnu",
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/local/lib",
        "/home/linuxbrew/.linuxbrew",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/gai.conf",
        "/etc/group",
        "/etc/ssl",
        "/etc/pki",
        "/etc/ca-certificates",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/localtime",
        "/etc/timezone",
        "/etc/alternatives",
        "/etc/os-release",
        "/etc/passwd",
        "/etc/services",
        "/usr/share",
        "/usr/share/locale",
        "/usr/share/zoneinfo",
        "/usr/share/terminfo",
        "/usr/share/ca-certificates",
        "/lib/terminfo",
        "/etc/terminfo",
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/full",
        "/dev/tty",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd",
        "/dev/pts",
        "/proc/self",
        "/proc/cpuinfo",
        "/proc/meminfo",
        "/proc/stat",
        "/proc/loadavg",
        "/proc/version",
        "/proc/filesystems",
    ]
}

#[cfg(target_os = "macos")]
pub(crate) fn write_paths() -> &'static [&'static str] {
    &[
        "/private/tmp",
        "/tmp",
        "/private/var/folders",
        "/var/folders",
        "/dev",
    ]
}

#[cfg(target_os = "linux")]
pub(crate) fn write_paths() -> &'static [&'static str] {
    &[
        "/tmp",
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd",
        "/dev/pts",
        "/proc/self/fd",
    ]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn read_paths() -> &'static [&'static str] {
    &[]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn write_paths() -> &'static [&'static str] {
    &[]
}
