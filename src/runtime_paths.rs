//! Private host-side directories writable by sandboxed commands.

use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};

pub(crate) fn workspace_cache(
    home: &Path,
    components: &[&str],
    workspace: &Path,
) -> Result<PathBuf> {
    let mut root = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    if !root.is_absolute() {
        bail!("XDG_CACHE_HOME must be an absolute path");
    }
    root.push("nonaut");
    for component in components {
        if !matches!(
            Path::new(component)
                .components()
                .collect::<Vec<_>>()
                .as_slice(),
            [Component::Normal(_)]
        ) {
            bail!("invalid internal runtime path component `{component}`");
        }
        root.push(component);
    }
    let mut hasher = DefaultHasher::new();
    workspace.hash(&mut hasher);
    Ok(root.join(format!("{:016x}", hasher.finish())))
}

pub(crate) fn prepare_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not protect {}", path.display()))
}
