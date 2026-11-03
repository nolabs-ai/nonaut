use super::super::{
    inventory::HostInventory,
    requirements::{Context, Requirements},
};
use anyhow::Result;

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "git",
    category: super::Category::Host,
    description: "Git without ambient system configuration",
    probe: Some(super::Probe {
        program: "git",
        args: &["--version"],
    }),
    markers: &[],
    detect: |_host| true,
    resolve: requirements,
};

pub(super) fn requirements(_host: &HostInventory, _context: &Context<'_>) -> Result<Requirements> {
    let mut plan = Requirements::default();
    plan.environment
        .insert("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned());
    Ok(plan)
}
