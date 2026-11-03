use super::super::{
    inventory::{HostInventory, Platform},
    requirements::{Context, Requirements},
};
use anyhow::{Result, ensure};

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "apple-sdk",
    category: super::Category::Host,
    description: "Selected Apple developer tools and private SDK cache",
    probe: Some(super::Probe {
        program: "xcrun",
        args: &["--show-sdk-path"],
    }),
    markers: &[],
    detect: |host| host.platform == Platform::Macos && !host.developer_directories.is_empty(),
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, context: &Context<'_>) -> Result<Requirements> {
    ensure!(
        host.platform == Platform::Macos,
        "apple-sdk is only available on macOS"
    );
    let mut plan = Requirements::default();
    for path in &host.developer_directories {
        plan.read_existing(path.clone());
    }
    if let Some(path) = host.developer_directories.first() {
        plan.environment
            .insert("DEVELOPER_DIR".into(), path.to_string_lossy().into_owned());
    }
    let cache = plan.private_directory(context.command_home.join("tmp/apple-sdk"));
    plan.environment.insert(
        "xcrun_db".to_owned(),
        cache.join("xcrun_db").to_string_lossy().into_owned(),
    );
    Ok(plan)
}
