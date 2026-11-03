use super::super::{
    inventory::HostInventory,
    requirements::{Context, Requirements},
};
use anyhow::Result;

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "go",
    category: super::Category::Toolchain,
    description: "Go installation and private build/module caches",
    probe: Some(super::Probe {
        program: "go",
        args: &["version"],
    }),
    markers: &["go.mod"],
    detect: |host| host.location("GOROOT").is_dir() || host.executable("go").is_some(),
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, context: &Context<'_>) -> Result<Requirements> {
    let mut plan = Requirements::default();
    let cache = plan.cache(context, "go")?;
    let gopath = plan.private_directory(cache.join("gopath"));
    let bin = plan.private_directory(gopath.join("bin"));
    plan.path_entries.push(bin);
    plan.bin(context.home.join("go/bin"));
    plan.read_existing(host.location("GOROOT").to_path_buf());
    plan.bin(host.location("GOROOT").join("bin"));
    for (name, path) in [
        ("GOPATH", gopath),
        ("GOCACHE", plan.private_directory(cache.join("build"))),
        ("GOMODCACHE", plan.private_directory(cache.join("mod"))),
    ] {
        plan.environment
            .insert(name.to_owned(), path.to_string_lossy().into_owned());
    }
    plan.environment
        .insert("GOENV".to_owned(), "off".to_owned());
    Ok(plan)
}
