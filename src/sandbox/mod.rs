//! OS sandbox support shared by command and structured-tool workers.

use std::path::{Path, PathBuf};

use crate::runtime::requirements::{Access, Grant};
use anyhow::{Context, Result};
use nono::{AccessMode, CapabilitySet};

pub(crate) fn apply_filesystem(
    mut capabilities: CapabilitySet,
    read: &[PathBuf],
    write: &[PathBuf],
    grants: &[Grant],
) -> Result<CapabilitySet> {
    for grant in grants {
        if let Some(expected) = &grant.canonical_path {
            anyhow::ensure!(
                std::fs::canonicalize(&grant.path).is_ok_and(|resolved| &resolved == expected),
                "runtime path {} changed after policy resolution; start a new session",
                grant.path.display()
            );
        }
    }
    for (paths, access) in [(read, AccessMode::Read), (write, AccessMode::ReadWrite)] {
        for path in paths {
            capabilities = allow_existing(capabilities, path, access)?;
        }
    }
    for grant in grants.iter().filter(|grant| grant.access == Access::Write) {
        capabilities = allow_existing(capabilities, &grant.path, AccessMode::Write)?;
    }
    Ok(capabilities)
}

fn allow_existing(
    capabilities: CapabilitySet,
    path: &Path,
    access: AccessMode,
) -> Result<CapabilitySet> {
    if path.is_dir() {
        capabilities
            .allow_path(path, access)
            .with_context(|| format!("could not grant access to {}", path.display()))
    } else if std::fs::canonicalize(path).is_ok() {
        capabilities
            .allow_file(path, access)
            .with_context(|| format!("could not grant access to {}", path.display()))
    } else {
        Ok(capabilities)
    }
}
