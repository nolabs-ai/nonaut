use super::super::{
    inventory::HostInventory,
    requirements::{Context, Requirements},
};
use anyhow::Result;

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "bun",
    category: super::Category::Toolchain,
    description: "Bun installation and private package cache",
    probe: Some(super::Probe {
        program: "bun",
        args: &["--version"],
    }),
    markers: &["bun.lock", "bun.lockb"],
    detect: |host| {
        host.location("BUN_INSTALL").join("bin/bun").is_file() || host.executable("bun").is_some()
    },
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, context: &Context<'_>) -> Result<Requirements> {
    let mut plan = Requirements::default();
    let cache = plan.cache(context, "bun")?;
    plan.read_existing(host.location("BUN_INSTALL").to_path_buf());
    plan.bin(host.location("BUN_INSTALL").join("bin"));
    plan.environment.insert(
        "BUN_INSTALL_CACHE_DIR".to_owned(),
        cache.to_string_lossy().into_owned(),
    );
    Ok(plan)
}
