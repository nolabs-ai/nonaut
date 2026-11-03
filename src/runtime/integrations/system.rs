use super::super::{
    inventory::{HostInventory, Platform},
    requirements::{Context, Requirements},
};
use anyhow::Result;
use std::path::PathBuf;

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "system",
    category: super::Category::System,
    description: "OS executables, loaders, certificates and devices",
    probe: Some(super::Probe {
        program: "sh",
        args: &["-c", "printf runtime-ok"],
    }),
    markers: &[],
    detect: |_host| true,
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, _context: &Context<'_>) -> Result<Requirements> {
    let (read, write): (&[&str], &[&str]) = match host.platform {
        Platform::Macos => (
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
                "/Library/Apple/System/Library",
                "/dev/null",
                "/dev/zero",
                "/dev/random",
                "/dev/urandom",
                "/dev/tty",
                "/dev/fd",
                "/dev/stdin",
                "/dev/stdout",
                "/dev/stderr",
                "/private/etc/ssl",
                "/private/var/select/sh",
                "/etc/ssl",
                "/etc/hosts",
                "/etc/resolv.conf",
                "/etc/localtime",
                "/usr/share/zoneinfo",
                "/usr/share/locale",
                "/var/db/timezone",
                "/usr/share/terminfo",
                "/System/Cryptexes",
            ],
            &[
                "/dev/null",
                "/dev/zero",
                "/dev/tty",
                "/dev/stdout",
                "/dev/stderr",
                "/dev/fd",
            ],
        ),
        Platform::Linux => (
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
            ],
            &[
                "/dev/null",
                "/dev/zero",
                "/dev/full",
                "/dev/tty",
                "/dev/stdout",
                "/dev/stderr",
                "/dev/fd",
                "/dev/pts",
                "/proc/self/fd",
            ],
        ),
    };
    let mut result = Requirements::default();
    for path in read {
        result.read_existing(PathBuf::from(path));
    }
    for path in write {
        if std::path::Path::new(path).exists() {
            result.write_only_paths.push(PathBuf::from(path));
        }
    }
    for path in &host.executable_paths {
        if !host.homebrew.iter().any(|prefix| path.starts_with(prefix)) {
            result.bin(path.clone());
        }
    }
    Ok(result)
}
