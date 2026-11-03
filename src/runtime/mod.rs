mod config;
pub(crate) mod inspection;
pub(crate) mod integrations;
mod inventory;
pub(crate) mod requirements;
#[cfg(test)]
mod tests;

use anyhow::{Result, bail, ensure};
pub(crate) use config::{Mode, RuntimeConfig};
use inventory::HostInventory;
use requirements::{Access, Context, Grant, GrantKind, Requirements};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Selection {
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Plan {
    pub mode: Mode,
    pub packs: Vec<Selection>,
    pub excluded: Vec<String>,
    pub grants: Vec<Grant>,
    pub path_entries: Vec<PathBuf>,
    #[serde(skip)]
    pub environment: BTreeMap<String, String>,
    pub environment_sources: BTreeMap<String, String>,
    pub diagnostics: Vec<String>,
    #[serde(skip)]
    private_directories: Vec<PathBuf>,
}

pub(crate) fn resolve(
    config: &RuntimeConfig,
    home: &Path,
    workspace: &Path,
    command_home: &Path,
) -> Result<Plan> {
    let host = if config.mode == Some(Mode::Custom) {
        HostInventory::empty(home, workspace)
    } else {
        HostInventory::discover(home, workspace)
    };
    resolve_with_host(config, &host, command_home)
}

fn resolve_with_host(
    config: &RuntimeConfig,
    host: &HostInventory,
    command_home: &Path,
) -> Result<Plan> {
    let mode = config.mode.unwrap_or_default();
    validate_names(&config.packs)?;
    validate_names(&config.exclude)?;
    ensure!(
        mode != Mode::Custom || (config.packs.is_empty() && config.exclude.is_empty()),
        "custom runtime mode does not accept packs or exclude; use explicit grants"
    );
    ensure!(
        mode == Mode::Auto || config.exclude.is_empty(),
        "runtime.exclude requires auto mode"
    );
    let mut plan = Plan {
        mode,
        packs: Vec::new(),
        excluded: config.exclude.clone(),
        grants: Vec::new(),
        path_entries: Vec::new(),
        environment: BTreeMap::new(),
        environment_sources: BTreeMap::new(),
        diagnostics: Vec::new(),
        private_directories: vec![command_home.to_path_buf(), command_home.join("tmp")],
    };
    let context = Context {
        home: &host.home,
        workspace: &host.workspace,
        command_home,
    };
    let mut definitions = integrations::CATALOG.iter().collect::<Vec<_>>();
    definitions.sort_by_key(|pack| {
        config
            .packs
            .iter()
            .position(|name| name == pack.name)
            .map_or((1, pack.category as usize), |index| (0, index))
    });
    for pack in definitions {
        let explicit = config.packs.iter().any(|name| name == pack.name);
        let marker = pack.project_marker(&host.workspace);
        let automatic = mode == Mode::Auto && (marker || pack.detected(host));
        if mode == Mode::Custom
            || config.exclude.iter().any(|name| name == pack.name)
            || !(explicit || automatic)
        {
            continue;
        }
        let reason = if explicit {
            "explicit selection"
        } else if marker {
            "project marker"
        } else {
            "host installation"
        };
        let requirements = pack.requirements(host, &context)?;
        plan.add_requirements(pack.name, requirements, host)?;
        plan.packs.push(Selection {
            name: pack.name.to_owned(),
            reason: reason.to_owned(),
        });
    }
    for (paths, access) in [
        (&config.filesystem.read, Access::Read),
        (&config.filesystem.readwrite, Access::ReadWrite),
    ] {
        for raw in paths {
            let path = explicit_path(raw, host)?;
            ensure!(
                path.exists(),
                "runtime grant {} does not exist; create it before granting access",
                path.display()
            );
            plan.grants.push(Grant {
                canonical_path: canonical_binding(&path)?,
                path,
                access,
                kind: GrantKind::Explicit,
                source: "runtime.filesystem".into(),
                reason: "explicit user grant".into(),
            });
        }
    }
    for (name, value) in &config.environment {
        validate_environment(name, value)?;
        if name == "PATH" {
            let entries = if value.is_empty() {
                Vec::new()
            } else {
                std::env::split_paths(value).collect::<Vec<_>>()
            };
            ensure!(
                entries.iter().all(|path| path.is_absolute()),
                "runtime.environment.PATH entries must be absolute"
            );
            plan.path_entries = entries;
        }
        plan.environment.insert(name.clone(), value.clone());
        plan.environment_sources
            .insert(name.clone(), "runtime.environment".into());
    }
    plan.grants.push(Grant {
        canonical_path: Some(command_home.to_path_buf()),
        path: command_home.to_path_buf(),
        access: Access::ReadWrite,
        kind: GrantKind::Executor,
        source: "executor".into(),
        reason: "private HOME, temporary files and command output".into(),
    });
    if mode == Mode::Custom {
        plan.diagnostics.push("Built-in runtime packs are disabled. Explicit PATH entries do not grant filesystem access.".into());
    }
    Ok(plan)
}

impl Plan {
    fn add_requirements(
        &mut self,
        source: &str,
        requirements: Requirements,
        host: &HostInventory,
    ) -> Result<()> {
        for (paths, access, kind) in [
            (requirements.read_paths, Access::Read, GrantKind::Runtime),
            (
                requirements.write_paths,
                Access::ReadWrite,
                GrantKind::PrivateState,
            ),
            (
                requirements.write_only_paths,
                Access::Write,
                GrantKind::Runtime,
            ),
        ] {
            for path in paths {
                // Descriptor aliases must bind to the worker, not this process.
                let canonical_path = if kind == GrantKind::PrivateState {
                    Some(path.clone())
                } else {
                    canonical_binding(&path)?
                };
                if kind == GrantKind::Runtime
                    && canonical_path.is_some()
                    && !matches!(source, "system" | "gcc")
                {
                    ensure!(
                        host.trusted_installation(&path),
                        "runtime pack {source} discovered an untrusted installation at {}; use an explicit runtime grant",
                        path.display()
                    );
                }
                let reason = match kind {
                    GrantKind::PrivateState => "isolated tool state",
                    _ => "tool execution support",
                }
                .to_owned();
                self.grants.push(Grant {
                    path,
                    canonical_path,
                    access,
                    kind,
                    source: source.to_owned(),
                    reason,
                });
            }
        }
        for path in requirements.path_entries {
            if !self.path_entries.contains(&path) {
                self.path_entries.push(path);
            }
        }
        for (name, value) in requirements.environment {
            if let Some(previous) = self.environment.get(&name) {
                ensure!(
                    previous == &value,
                    "conflicting runtime environment variable {name} from {source}"
                );
            }
            self.environment.insert(name.clone(), value);
            self.environment_sources.insert(name, source.to_owned());
        }
        self.private_directories
            .extend(requirements.private_directories);
        Ok(())
    }

    pub fn prepare(&self) -> Result<()> {
        for path in &self.private_directories {
            crate::runtime_paths::prepare_private_directory(path)?;
        }
        Ok(())
    }

    pub fn validate_writes(&self, write_paths: &[PathBuf]) -> Result<()> {
        for grant in self
            .grants
            .iter()
            .filter(|grant| grant.kind == GrantKind::Runtime && !grant.access.writable())
        {
            let path = grant.canonical_path.as_ref().unwrap_or(&grant.path);
            for write in write_paths {
                ensure!(
                    !path.starts_with(write) && !write.starts_with(path),
                    "runtime installation {} is writable through {}; use an explicit runtime grant instead",
                    grant.path.display(),
                    write.display()
                );
            }
        }
        Ok(())
    }
}

fn canonical_binding(path: &Path) -> Result<Option<PathBuf>> {
    if path.starts_with("/proc/self")
        || path.starts_with("/dev/fd")
        || matches!(
            path.to_str(),
            Some("/dev/stdin" | "/dev/stdout" | "/dev/stderr")
        )
    {
        return Ok(None);
    }
    crate::policy::canonicalize_nearest(path).map(Some)
}

fn explicit_path(raw: &str, host: &HostInventory) -> Result<PathBuf> {
    ensure!(
        !raw.trim().is_empty() && !raw.contains(['*', '?']),
        "runtime filesystem entries must be nonempty literal paths"
    );
    let path = if raw == "~" {
        host.home.clone()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        host.home.join(rest)
    } else {
        PathBuf::from(raw)
    };
    Ok(if path.is_absolute() {
        path
    } else {
        host.workspace.join(path)
    })
}

fn validate_names(names: &[String]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        ensure!(
            seen.insert(name),
            "runtime pack `{name}` is listed more than once"
        );
        ensure!(
            integrations::CATALOG.iter().any(|pack| pack.name == name),
            "unsupported runtime pack `{name}`; available packs: {}",
            integrations::CATALOG
                .iter()
                .map(|pack| pack.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn validate_environment(name: &str, value: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .enumerate()
                .all(|(index, ch)| ch.is_ascii_alphabetic()
                    || ch == b'_'
                    || index > 0 && ch.is_ascii_digit())
            && !value.contains('\0'),
        "invalid runtime environment variable {name}"
    );
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("NONAUT_")
        || matches!(
            upper.as_str(),
            "HOME"
                | "TMPDIR"
                | "TMP"
                | "TEMP"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
        )
    {
        bail!("runtime environment variable {name} is reserved by the executor");
    }
    Ok(())
}
