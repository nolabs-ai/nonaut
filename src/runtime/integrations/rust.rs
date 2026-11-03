use super::super::{
    inventory::HostInventory,
    requirements::{Context, Requirements},
};
use anyhow::Result;

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "rust",
    category: super::Category::Toolchain,
    description: "Rust toolchain and private Cargo cache",
    probe: Some(super::Probe {
        program: "cargo",
        args: &["--version"],
    }),
    markers: &["Cargo.toml"],
    detect: |host| {
        host.location("RUSTUP_HOME").is_dir()
            || host.location("CARGO_HOME").join("bin/cargo").is_file()
            || host.executable("cargo").is_some()
    },
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, context: &Context<'_>) -> Result<Requirements> {
    let mut plan = Requirements::default();
    let cache = plan.cache(context, "rust")?;
    let bundled = plan.private_directory(cache.join("bundled-cli"));
    plan.bin(host.location("CARGO_HOME").join("bin"));
    plan.read_existing(host.location("RUSTUP_HOME").to_path_buf());
    for (name, path) in [
        ("CARGO_HOME", cache),
        ("RUSTUP_HOME", host.location("RUSTUP_HOME").to_path_buf()),
        ("BUNDLED_CLI_CACHE_DIR", bundled),
    ] {
        plan.environment
            .insert(name.to_owned(), path.to_string_lossy().into_owned());
    }
    Ok(plan)
}
