use super::super::{
    inventory::HostInventory,
    requirements::{Context, Requirements},
};
use anyhow::Result;

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "python",
    category: super::Category::Toolchain,
    description: "Python installations and private package caches",
    probe: Some(super::Probe {
        program: "python3",
        args: &["-c", "import sys; print(sys.version.split()[0])"],
    }),
    markers: &["pyproject.toml", "requirements.txt"],
    detect: |host| host.location("PYENV_ROOT").is_dir() || host.executable("python3").is_some(),
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, context: &Context<'_>) -> Result<Requirements> {
    let mut plan = Requirements::default();
    let cache = plan.cache(context, "python")?;
    for path in [
        host.location("PYENV_ROOT").to_path_buf(),
        context.home.join(".local/bin"),
        context.home.join(".local/share/uv"),
    ] {
        plan.read_existing(path);
    }
    for path in [
        host.location("PYENV_ROOT").join("shims"),
        host.location("PYENV_ROOT").join("bin"),
        context.home.join(".local/bin"),
    ] {
        plan.bin(path);
    }
    if host.platform == super::super::inventory::Platform::Linux
        && let Ok(versions) = std::fs::read_dir("/usr/lib")
    {
        for version in versions.flatten() {
            let name = version.file_name();
            if name
                .to_str()
                .and_then(|name| name.strip_prefix("python"))
                .is_some_and(|version| {
                    !version.is_empty()
                        && version.bytes().all(|ch| ch.is_ascii_digit() || ch == b'.')
                })
            {
                plan.read_existing(version.path().join("sitecustomize.py"));
            }
        }
    }
    for name in ["PIP_CACHE_DIR", "UV_CACHE_DIR", "PYTHONPYCACHEPREFIX"] {
        plan.environment
            .insert(name.to_owned(), cache.to_string_lossy().into_owned());
    }
    plan.environment.insert(
        "PYENV_ROOT".to_owned(),
        host.location("PYENV_ROOT").to_string_lossy().into_owned(),
    );
    Ok(plan)
}
