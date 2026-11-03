use super::super::{
    inventory::{HostInventory, Platform},
    requirements::{Context, Requirements},
};
use anyhow::{Result, ensure};
use std::path::PathBuf;

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "gcc",
    category: super::Category::Host,
    description: "Linux compiler helpers and public development headers",
    probe: Some(super::Probe {
        program: "cc",
        args: &["--version"],
    }),
    markers: &[],
    detect: |host| host.platform == Platform::Linux && host.executable("cc").is_some(),
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, _context: &Context<'_>) -> Result<Requirements> {
    ensure!(
        host.platform == Platform::Linux,
        "gcc runtime helpers are only applicable on Linux"
    );
    let mut plan = Requirements::default();
    for path in ["/usr/libexec/gcc", "/usr/include", "/usr/local/include"] {
        plan.read_existing(PathBuf::from(path));
    }
    Ok(plan)
}
