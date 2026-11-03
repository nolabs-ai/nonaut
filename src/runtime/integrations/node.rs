use super::super::{
    inventory::HostInventory,
    requirements::{Context, Requirements},
};
use anyhow::Result;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub(super) const DEFINITION: super::Integration = super::Integration {
    name: "node",
    category: super::Category::Toolchain,
    description: "Node installations and private package caches",
    probe: Some(super::Probe {
        program: "node",
        args: &["-e", "console.log(process.version)"],
    }),
    markers: &["package.json"],
    detect: |host| {
        host.location("NVM_DIR").is_dir()
            || host.location("FNM_DIR").is_dir()
            || host.home.join(".local/share/fnm").is_dir()
            || host.executable("node").is_some()
    },
    resolve: requirements,
};

pub(super) fn requirements(host: &HostInventory, context: &Context<'_>) -> Result<Requirements> {
    let mut plan = Requirements::default();
    let cache = plan.cache(context, "node")?;
    let nvm = host.location("NVM_DIR");
    let fnm = host.location("FNM_DIR");
    let fnm_data = context.home.join(".local/share/fnm");
    let roots = [
        nvm.to_path_buf(),
        fnm.to_path_buf(),
        fnm_data.clone(),
        context.home.join(".node"),
        context.home.join(".npm-global"),
        context.home.join("Library/pnpm"),
        context.home.join(".local/share/pnpm"),
    ];
    for path in &roots {
        plan.read_existing(path.clone());
    }
    if let Some(path) = env::var_os("PATH") {
        let canonical_roots = roots
            .iter()
            .filter_map(|root| fs::canonicalize(root).ok())
            .collect::<Vec<_>>();
        for entry in env::split_paths(&path) {
            if let Ok(canonical) = fs::canonicalize(&entry)
                && canonical_roots
                    .iter()
                    .any(|root| canonical.starts_with(root))
            {
                plan.bin(canonical);
            }
        }
    }
    for (parent, suffix) in [
        (nvm.join("versions/node"), "bin"),
        (fnm.join("node-versions"), "installation/bin"),
        (fnm_data.join("node-versions"), "installation/bin"),
    ] {
        for bin in version_bins(&parent, Path::new(suffix)) {
            plan.bin(bin);
        }
    }
    for path in [
        context.home.join(".node/bin"),
        context.home.join(".npm-global/bin"),
        context.home.join("Library/pnpm"),
        context.home.join(".local/share/pnpm"),
    ] {
        plan.bin(path);
    }
    for (name, child) in [
        ("npm_config_cache", "npm"),
        ("YARN_CACHE_FOLDER", "yarn"),
        ("COREPACK_HOME", "corepack"),
        ("npm_config_store_dir", "pnpm-store"),
    ] {
        let directory = plan.private_directory(cache.join(child));
        plan.environment
            .insert(name.to_owned(), directory.to_string_lossy().into_owned());
    }
    if nvm.is_dir() {
        plan.environment
            .insert("NVM_DIR".to_owned(), nvm.to_string_lossy().into_owned());
    }
    Ok(plan)
}

pub(super) fn version_bins(parent: &Path, suffix: &Path) -> Vec<PathBuf> {
    let Ok(canonical_parent) = fs::canonicalize(parent) else {
        return Vec::new();
    };
    let Ok(children) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut bins = children
        .filter_map(Result::ok)
        .map(|child| child.path().join(suffix))
        .filter_map(|path| fs::canonicalize(path).ok())
        .filter(|path| path.is_dir() && path.starts_with(&canonical_parent))
        .collect::<Vec<_>>();
    bins.sort_unstable_by(|left, right| right.cmp(left));
    bins.dedup();
    bins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_discovery_stays_within_the_installation() {
        let root = std::env::temp_dir().join(format!("nonaut-node-{}", std::process::id()));
        let versions = root.join("versions");
        let inside = versions.join("v2/bin");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(root.join("outside/bin")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), versions.join("v1")).unwrap();
        assert_eq!(
            version_bins(&versions, Path::new("bin")),
            [fs::canonicalize(&inside).unwrap()]
        );
        fs::remove_dir_all(root).unwrap();
    }
}
