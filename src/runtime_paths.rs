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
    root = crate::policy::canonicalize_nearest(&root)?;
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
    use nix::{
        fcntl::{OFlag, open, openat},
        sys::stat::{Mode, mkdirat},
    };
    bail_if_relative(path)?;
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut directory = open(Path::new("/"), flags, Mode::empty())?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        match mkdirat(&directory, name, Mode::S_IRWXU) {
            Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("could not create {}", path.display()));
            }
        }
        directory = openat(&directory, name, flags, Mode::empty()).with_context(|| {
            format!(
                "private directory {} contains a symlink or inaccessible component",
                path.display()
            )
        })?;
    }
    let file = fs::File::from(directory);
    file.set_permissions(fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not protect {}", path.display()))
}

fn bail_if_relative(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!(
            "private directory must be an absolute normalized path: {}",
            path.display()
        );
    }
    Ok(())
}
